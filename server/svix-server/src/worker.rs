// SPDX-FileCopyrightText: © 2022 Svix Authors
// SPDX-Licensepub(crate) -Identifier: MIT

use crate::cfg::Configuration;

use crate::core::cache::{kv_def, Cache, CacheBehavior, CacheKey, CacheValue};
use crate::core::cryptography::Encryption;
use crate::core::message_app::{CreateMessageApp, CreateMessageEndpoint};
use crate::core::operational_webhooks::{
    EndpointDisabledEvent, MessageAttemptEvent, OperationalWebhook, OperationalWebhookSender,
};
use crate::core::types::{
    ApplicationId, ApplicationUid, BaseId, EndpointHeaders, EndpointId, EndpointSecretInternal,
    EndpointSecretType, MessageAttemptId, MessageAttemptTriggerType, MessageId, MessageStatus,
    MessageUid, OrganizationId,
};
use crate::core::webhook_http_client::{
    Error as WebhookClientError, RequestBuilder, WebhookClient,
};
use crate::db::models::{endpoint, message, messageattempt, messagedestination};
use crate::error::{Error, ErrorType, HttpError, Result};
use crate::queue::{
    MessageTask, MessageTaskBatch, QueueTask, TaskQueueConsumer, TaskQueueProducer,
};
use crate::v1::utils::get_unix_timestamp;
use crate::{ctx, err_cache, err_generic, err_validation};

use chrono::Utc;

use futures::future;
use http::{HeaderValue, StatusCode, Version};
use ipnet::IpNet;
use lazy_static::lazy_static;
use rand::Rng;

use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, TryIntoModel,
};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::Instrument;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub type CaseSensitiveHeaderMap = HashMap<String, HeaderValue>;

// The maximum variation from the retry schedule when applying jitter to a resent webhook event in
// percent deviation
const JITTER_DELTA: f32 = 0.2;
const OVERLOAD_PENALTY_SECS: u64 = 60;

const USER_AGENT: &str = concat!("Svix-Webhooks/", env!("CARGO_PKG_VERSION"));

/// Send the MessageAttemptFailingEvent after exceeding this number of failed attempts
const OP_WEBHOOKS_SEND_FAILING_EVENT_AFTER: usize = 4;

const RESPONSE_MAX_SIZE: usize = 20000;

/// A simple struct noting the context of the wrapped [`DateTimeUtc`]. This struct is returned when
/// you are to disable disable an endpoint. This is optionally returned by [`process_failure_cache`]
/// which is to be called after all retry events are exhausted.
#[repr(transparent)]
struct EndpointDisableInfo {
    first_failure_at: DateTimeUtc,
}

/// The first_failure_at time is only stored in Postgres after the endpoint has been disabled.
/// Otherwise, it is stored in the cache with an expiration.
#[derive(Deserialize, Serialize)]
pub struct FailureCacheValue {
    pub first_failure_at: DateTimeUtc,
}

kv_def!(FailureCacheKey, FailureCacheValue, "SVIX_FAILURE_CACHE");

impl FailureCacheKey {
    pub fn new(
        org_id: &OrganizationId,
        app_id: &ApplicationId,
        endp_id: &EndpointId,
    ) -> FailureCacheKey {
        FailureCacheKey(format!(
            "{}_{}_{}_{}",
            Self::PREFIX_CACHE,
            org_id,
            app_id,
            endp_id
        ))
    }
}

/// Called upon the successful dispatch of an endpoint. Simply clears the cache of a
/// [`FailureCacheKey`]/[`FailureCacheValue`] pair associated with a given endpoint. This is such
/// that an endpoint that was previously not responding is not disabled after responding again.
///
/// If the key value pair does not already exist in the cache, indicating that the endpoint never
/// stopped responding, no operation is performed.
#[tracing::instrument(skip_all)]
async fn process_endpoint_success(
    cache: &Cache,
    app_id: &ApplicationId,
    org_id: &OrganizationId,
    endp: &CreateMessageEndpoint,
) -> Result<()> {
    let key = FailureCacheKey::new(org_id, app_id, &endp.id);

    cache.delete(&key).await.map_err(|e| err_cache!(e))
}

/// Called upon endpoint failure. Returns whether to disable the endpoint based on the time of first
/// failure stored in the cache.
///
/// If no failure has previously been reported, then now is cached as the time of first failure and
/// the endpoint is not disabled.
///
/// If there has been a  preivous failure, then it is compared to the configured grace period, where
/// if there have been only failures within the grace period, then the endpoint is disabled.
///
/// All cache values are set with an expiration time greater thah the grace period, so occasional
/// failures will not cause an endpoint to be disabled.
#[tracing::instrument(skip_all)]
async fn process_endpoint_failure(
    cache: &Cache,
    app_id: &ApplicationId,
    org_id: &OrganizationId,
    endp: &CreateMessageEndpoint,
    disable_in: Duration,
) -> Result<Option<EndpointDisableInfo>> {
    let key = FailureCacheKey::new(org_id, app_id, &endp.id);
    let now = Utc::now();

    // If it already exists in the cache, see if the grace preiod has already elapsed
    if let Some(FailureCacheValue { first_failure_at }) = cache
        .get::<FailureCacheValue>(&key)
        .await
        .map_err(|e| err_generic!(e))?
    {
        if now - first_failure_at
            > chrono::Duration::from_std(disable_in).expect("Given `disable_in` is too large")
        {
            Ok(Some(EndpointDisableInfo { first_failure_at }))
        } else {
            Ok(None)
        }
    }
    // If it does not yet exist in the cache, set the first_failure_at value to now
    else {
        cache
            .set(
                &key,
                &FailureCacheValue {
                    first_failure_at: now,
                },
                // Failures are forgiven after double the `disable_in` `Duration` with the expiry of
                // the Redis key
                disable_in * 2,
            )
            .await
            .map_err(|e| err_generic!(e))?;

        Ok(None)
    }
}

/// Sign a message
fn sign_msg(
    main_secret: &Encryption,
    timestamp: i64,
    body: &str,
    msg_id: &MessageId,
    endpoint_signing_keys: &[&EndpointSecretInternal],
) -> String {
    let to_sign = format!("{msg_id}.{timestamp}.{body}");
    endpoint_signing_keys
        .iter()
        .map(|x| {
            let sig = x.sign(main_secret, to_sign.as_bytes());
            let version = match x.type_() {
                EndpointSecretType::Hmac256 => "v1",
                EndpointSecretType::Ed25519 => "v1a",
            };
            format!("{},{}", version, base64::encode(sig))
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Generates a set of headers for any one webhook event
fn generate_msg_headers(
    timestamp: i64,
    msg_id: &MessageId,
    signatures: String,
    whitelabel_headers: bool,
    configured_headers: Option<&EndpointHeaders>,
    _endpoint_url: &str,
) -> Result<CaseSensitiveHeaderMap> {
    let mut headers = CaseSensitiveHeaderMap::new();
    let id_hdr = msg_id
        .0
        .parse()
        .map_err(|_| err_generic!("Error parsing message id".to_string()))?;
    let timestamp = timestamp
        .to_string()
        .parse()
        .map_err(|_| err_generic!("Error parsing message timestamp".to_string()))?;
    let signatures_str = signatures
        .parse()
        .map_err(|_| err_generic!("Error parsing message signatures".to_string()))?;
    if whitelabel_headers {
        headers.insert("webhook-id".to_owned(), id_hdr);
        headers.insert("webhook-timestamp".to_owned(), timestamp);
        headers.insert("webhook-signature".to_owned(), signatures_str);
    } else {
        headers.insert("svix-id".to_owned(), id_hdr);
        headers.insert("svix-timestamp".to_owned(), timestamp);
        headers.insert("svix-signature".to_owned(), signatures_str);
    }
    headers.insert(
        "user-agent".to_owned(),
        USER_AGENT.to_string().parse().unwrap(),
    );
    headers.insert(
        "content-type".to_owned(),
        "application/json".parse().unwrap(),
    );
    if let Some(configured_headers) = configured_headers {
        for (k, v) in &configured_headers.0 {
            match v.parse() {
                Ok(v) => {
                    headers.insert(k.clone(), v);
                }
                Err(e) => {
                    tracing::error!("Invalid HeaderValue {}: {}", v, e);
                }
            }
        }
    }

    Ok(headers)
}

#[derive(Clone)]
struct WorkerContext<'a> {
    cfg: &'a Configuration,
    cache: &'a Cache,
    db: &'a DatabaseConnection,
    queue_tx: &'a TaskQueueProducer,
    op_webhook_sender: &'a OperationalWebhookSender,
}

struct FailedDispatch(messageattempt::ActiveModel, Error);
struct SuccessfulDispatch(messageattempt::ActiveModel);

#[allow(clippy::large_enum_variant)]
enum IncompleteDispatch {
    Pending(PendingDispatch),
    #[allow(dead_code)]
    Failed(FailedDispatch),
}

struct PendingDispatch {
    method: http::Method,
    url: String,
    headers: CaseSensitiveHeaderMap,
    payload: String,
    request_timeout: u64,
    created_at: DateTimeUtc,
}

enum CompletedDispatch {
    Failed(FailedDispatch),
    Successful(SuccessfulDispatch),
}

#[tracing::instrument(skip_all)]
async fn prepare_dispatch(
    WorkerContext { cfg, .. }: &WorkerContext<'_>,
    DispatchContext {
        msg_task,
        payload,
        endp,
        ..
    }: DispatchContext<'_>,
) -> Result<IncompleteDispatch> {
    let attempt_created_at = Utc::now();

    let headers = {
        let keys = endp.valid_signing_keys();

        let signatures = sign_msg(
            &cfg.encryption,
            attempt_created_at.timestamp(),
            payload,
            &msg_task.msg_id,
            &keys,
        );

        generate_msg_headers(
            attempt_created_at.timestamp(),
            &msg_task.msg_id,
            signatures,
            cfg.whitelabel_headers,
            endp.headers.as_ref(),
            &endp.url,
        )?
    };

    Ok(IncompleteDispatch::Pending(PendingDispatch {
        method: http::Method::POST,
        url: endp.url.clone(),
        headers,
        payload: payload.to_owned(),
        request_timeout: cfg.worker_request_timeout as _,
        created_at: attempt_created_at,
    }))
}

#[tracing::instrument(skip_all)]
async fn make_http_call(
    DispatchContext { msg_task, endp, .. }: DispatchContext<'_>,
    PendingDispatch {
        method,
        url,
        headers,
        payload,
        request_timeout,
        created_at,
    }: PendingDispatch,
    msg_dest: &messagedestination::Model,
    whitelist_subnets: &Option<Arc<Vec<IpNet>>>,
) -> Result<CompletedDispatch> {
    let client = WebhookClient::new(
        whitelist_subnets.clone(),
        Some(Arc::new(vec!["backend".to_owned()])),
    );
    let req = RequestBuilder::new()
        .method(method)
        .uri_str(&url)
        .map_err(|_| err_validation!("URL is invalid".to_owned()))?
        .headers(headers)
        .body(payload.into(), HeaderValue::from_static("application/json"))
        .version(Version::HTTP_11)
        .timeout(Duration::from_secs(request_timeout))
        .build()
        .map_err(|e| err_generic!(e))?;

    let attempt = messageattempt::ActiveModel {
        // Set both ID and created_at to the same timestamp
        id: Set(MessageAttemptId::new(created_at.into(), None)),
        created_at: Set(created_at.into()),
        msg_id: Set(msg_task.msg_id.clone()),
        endp_id: Set(endp.id.clone()),
        msg_dest_id: Set(msg_dest.id.clone()),
        url: Set(endp.url.clone()),
        ended_at: Set(Some(Utc::now().into())),
        trigger_type: Set(msg_task.trigger_type),
        ..Default::default()
    };

    match client.execute(req).await {
        Ok(res) => {
            let status_code = res.status().as_u16() as i16;
            let status = if res.status().is_success() {
                MessageStatus::Success
            } else {
                MessageStatus::Fail
            };

            let http_error = if !res.status().is_success() {
                Some(WebhookClientError::FailureStatus(res.status()))
            } else {
                None
            };

            let body = match hyper::body::to_bytes(res.into_body()).await {
                Ok(bytes) if bytes.len() > RESPONSE_MAX_SIZE => {
                    bytes_to_string(bytes.slice(..RESPONSE_MAX_SIZE))
                }
                Ok(bytes) => bytes_to_string(bytes),
                Err(err) => format!("Error reading response body: {err}"),
            };

            let attempt = messageattempt::ActiveModel {
                response_status_code: Set(status_code),
                response: Set(body),
                status: Set(status),
                ..attempt
            };

            match http_error {
                Some(err) => Ok(CompletedDispatch::Failed(FailedDispatch(
                    attempt,
                    err_generic!(err.to_string()),
                ))),
                None => Ok(CompletedDispatch::Successful(SuccessfulDispatch(attempt))),
            }
        }
        Err(err) => Ok(CompletedDispatch::Failed(FailedDispatch(
            messageattempt::ActiveModel {
                response_status_code: Set(0),
                response: Set(err.to_string()),
                status: Set(MessageStatus::Fail),
                ..attempt
            },
            err.into(),
        ))),
    }
}

#[tracing::instrument(skip_all, fields(response_code, msg_dest_id=msg_dest.id.0))]
async fn handle_successful_dispatch(
    WorkerContext { cache, db, .. }: &WorkerContext<'_>,
    DispatchContext {
        org_id,
        endp,
        app_id,
        ..
    }: DispatchContext<'_>,
    SuccessfulDispatch(mut attempt): SuccessfulDispatch,
    msg_dest: messagedestination::Model,
) -> Result<()> {
    attempt.ended_at = Set(Some(Utc::now().into()));
    let attempt = ctx!(attempt.insert(*db).await)?;

    let msg_dest = messagedestination::ActiveModel {
        status: Set(MessageStatus::Success),
        next_attempt: Set(None),
        ..msg_dest.into()
    };
    let _msg_dest = ctx!(msg_dest.update(*db).await)?;

    process_endpoint_success(cache, app_id, org_id, endp).await?;

    tracing::Span::current().record("response_code", attempt.response_status_code);
    tracing::info!("Webhook success.");

    Ok(())
}

fn calculate_retry_delay(duration: Duration, err: Error) -> Duration {
    let duration = if matches!(err.typ, ErrorType::Timeout(_))
        || matches!(err.typ, ErrorType::Http(HttpError { status, .. }) if status == StatusCode::TOO_MANY_REQUESTS)
    {
        std::cmp::max(duration, Duration::from_secs(OVERLOAD_PENALTY_SECS))
    } else {
        duration
    };
    // Apply jitter with a maximum variation of JITTER_DELTA
    rand::thread_rng()
        .gen_range(duration.mul_f32(1.0 - JITTER_DELTA)..=duration.mul_f32(1.0 + JITTER_DELTA))
}

#[tracing::instrument(skip_all, fields(response_code, msg_dest_id=msg_dest.id.0))]
async fn handle_failed_dispatch(
    WorkerContext {
        db,
        cache,
        op_webhook_sender,
        cfg,
        queue_tx,
        ..
    }: &WorkerContext<'_>,
    DispatchContext {
        org_id,
        app_id,
        app_uid,
        msg_uid,
        endp,
        msg_task,
        ..
    }: DispatchContext<'_>,
    FailedDispatch(mut attempt, err): FailedDispatch,
    msg_dest: messagedestination::Model,
) -> Result<()> {
    attempt.ended_at = Set(Some(Utc::now().into()));
    let attempt = ctx!(attempt.insert(*db).await)?;

    tracing::Span::current().record("response_code", attempt.response_status_code);
    tracing::info!("Webhook failure.");

    let retry_schedule = &cfg.retry_schedule;

    let attempt_count = msg_task.attempt_count as usize;
    if msg_task.trigger_type == MessageAttemptTriggerType::Manual {
        tracing::debug!("Manual retry failed");
        Ok(())
    } else if attempt_count < retry_schedule.len() {
        tracing::debug!(
            "Worker failure retrying for attempt {}: {} {} {}",
            attempt_count,
            err,
            &msg_dest.id,
            &endp.id
        );

        let retry_delay = calculate_retry_delay(retry_schedule[attempt_count], err);
        let next_attempt_time =
            Utc::now() + chrono::Duration::from_std(retry_delay).expect("Error parsing duration");
        let msg_dest = messagedestination::ActiveModel {
            next_attempt: Set(Some(next_attempt_time.into())),
            ..msg_dest.into()
        };
        let _msg_dest = ctx!(msg_dest.update(*db).await)?;

        if attempt_count == (OP_WEBHOOKS_SEND_FAILING_EVENT_AFTER - 1) {
            if let Err(e) = op_webhook_sender
                .send_operational_webhook(
                    org_id,
                    OperationalWebhook::MessageAttemptFailing(MessageAttemptEvent {
                        app_id: app_id.clone(),
                        app_uid: app_uid.cloned(),
                        endpoint_id: msg_task.endpoint_id.clone(),
                        msg_id: msg_task.msg_id.clone(),
                        msg_event_id: msg_uid.cloned(),
                        last_attempt: attempt.into(),
                    }),
                )
                .await
            {
                tracing::error!(
                    "Failed sending MessageAttemptFailing Operational Webhook: {}",
                    e
                );
            }
        }
        queue_tx
            .send(
                QueueTask::MessageV1(MessageTask {
                    attempt_count: msg_task.attempt_count + 1,
                    ..msg_task.clone()
                }),
                Some(retry_delay),
            )
            .await?;

        Ok(())
    } else {
        tracing::debug!(
            "Worker failure attempts exhausted: {} {} {}",
            err,
            &msg_dest.id,
            &endp.id
        );

        let msg_dest = messagedestination::ActiveModel {
            status: Set(MessageStatus::Fail),
            next_attempt: Set(None),
            ..msg_dest.into()
        };
        let _msg_dest = ctx!(msg_dest.update(*db).await)?;

        // Send common operational webhook
        op_webhook_sender
            .send_operational_webhook(
                org_id,
                OperationalWebhook::MessageAttemptExhausted(MessageAttemptEvent {
                    app_id: app_id.clone(),
                    app_uid: app_uid.cloned(),
                    endpoint_id: msg_task.endpoint_id.clone(),
                    msg_id: msg_task.msg_id.clone(),
                    msg_event_id: msg_uid.cloned(),
                    last_attempt: attempt.into(),
                }),
            )
            .await?;

        match process_endpoint_failure(
            cache,
            app_id,
            org_id,
            endp,
            cfg.endpoint_failure_disable_after,
        )
        .await?
        {
            None => Ok(()),

            Some(EndpointDisableInfo { first_failure_at }) => {
                let endp = ctx!(
                    endpoint::Entity::secure_find_by_id(
                        msg_task.app_id.clone(),
                        msg_task.endpoint_id.clone(),
                    )
                    .one(*db)
                    .await
                )?
                .ok_or_else(|| {
                    err_generic!("Endpoint not found {} {}", app_id, &msg_task.endpoint_id)
                })?;

                let endp = endpoint::ActiveModel {
                    disabled: Set(true),
                    first_failure_at: Set(Some(first_failure_at.into())),
                    ..endp.into()
                };
                let _endp = ctx!(endp.update(*db).await)?;

                // Send operational webhooks
                op_webhook_sender
                    .send_operational_webhook(
                        org_id,
                        OperationalWebhook::EndpointDisabled(EndpointDisabledEvent {
                            app_id: app_id.clone(),
                            app_uid: app_uid.cloned(),
                            endpoint_id: msg_task.endpoint_id.clone(),
                            // TODO:
                            endpoint_uid: None,
                            fail_since: first_failure_at,
                        }),
                    )
                    .await
            }
        }
    }
}

#[derive(Clone)]
struct DispatchContext<'a> {
    msg_task: &'a MessageTask,
    payload: &'a str,
    endp: &'a CreateMessageEndpoint,
    org_id: &'a OrganizationId,
    app_id: &'a ApplicationId,
    app_uid: Option<&'a ApplicationUid>,
    msg_uid: Option<&'a MessageUid>,
}

/// Dispatches one webhook
#[tracing::instrument(
    skip_all,
    level = "error",
    fields(
        endp_id = msg_task.endpoint_id.0.as_str(),
    )
)]
async fn dispatch_message_task(
    worker_context: &WorkerContext<'_>,
    msg: &message::Model,
    app: &CreateMessageApp,
    msg_task: MessageTask,
    payload: &str,
    endp: CreateMessageEndpoint,
    msg_dest: Option<messagedestination::Model>,
) -> Result<()> {
    let WorkerContext { cfg, db, .. } = worker_context;

    tracing::trace!("Dispatch start");

    let msg_dest = if let Some(msg_dest) = msg_dest {
        msg_dest
    } else {
        ctx!(
            messagedestination::Entity::secure_find_by_msg(msg_task.msg_id.clone())
                .filter(messagedestination::Column::EndpId.eq(endp.id.clone()))
                .one(*db)
                .await
        )?
        .ok_or_else(|| err_generic!("Msg dest not found {} {}", msg_task.msg_id, endp.id))?
    };

    if (msg_dest.status != MessageStatus::Pending && msg_dest.status != MessageStatus::Sending)
        && (msg_task.trigger_type != MessageAttemptTriggerType::Manual)
    {
        // TODO: it happens when this message destination is "resent". This leads to 2 queue tasks with the same message destination
        tracing::warn!(
            "MessageDestination {} is not pending (it's {:?}).",
            msg_dest.id,
            msg_dest.status
        );
        return Ok(());
    }

    let dispatch_context = DispatchContext {
        msg_task: &msg_task,
        payload,
        endp: &endp,
        org_id: &app.org_id,
        app_id: &app.id,
        app_uid: app.uid.as_ref(),
        msg_uid: msg.uid.as_ref(),
    };

    let dispatch = prepare_dispatch(worker_context, dispatch_context.clone()).await?;
    let completed = match dispatch {
        IncompleteDispatch::Pending(pending) => {
            make_http_call(
                dispatch_context.clone(),
                pending,
                &msg_dest,
                &cfg.whitelist_subnets,
            )
            .await?
        }
        IncompleteDispatch::Failed(failed) => CompletedDispatch::Failed(failed),
    };

    match completed {
        CompletedDispatch::Successful(success) => {
            handle_successful_dispatch(worker_context, dispatch_context, success, msg_dest).await
        }
        CompletedDispatch::Failed(failed) => {
            handle_failed_dispatch(worker_context, dispatch_context, failed, msg_dest).await
        }
    }
}

fn bytes_to_string(bytes: bytes::Bytes) -> String {
    match std::str::from_utf8(&bytes) {
        Ok(v) => v.to_owned(),
        Err(_) => base64::encode(&bytes),
    }
}

/// Manages preparation and execution of a QueueTask type
#[tracing::instrument(skip_all, level = "error", fields(msg_id, app_id, org_id, instance_id, task_type=queue_task.to_string()))]
async fn process_queue_task(
    worker_context: WorkerContext<'_>,
    queue_task: QueueTask,
) -> Result<()> {
    process_queue_task_inner(worker_context, queue_task)
        .await
        .map_err(|e| {
            tracing::error!("{e}");
            e
        })
}

/// Manages preparation and execution of a QueueTask type
async fn process_queue_task_inner(
    worker_context: WorkerContext<'_>,
    queue_task: QueueTask,
) -> Result<()> {
    let WorkerContext { db, cache, .. }: WorkerContext<'_> = worker_context;

    if queue_task == QueueTask::HealthCheck {
        return Ok(());
    }

    let span = tracing::Span::current();

    let (msg_id, trigger_type) = match &queue_task {
        QueueTask::MessageBatch(MessageTaskBatch {
            msg_id,
            trigger_type,
            ..
        }) => (msg_id, trigger_type),
        QueueTask::MessageV1(MessageTask {
            msg_id,
            trigger_type,
            ..
        }) => (msg_id, trigger_type),

        QueueTask::HealthCheck => unreachable!(),
    };

    span.record("msg_id", &msg_id.0);

    let msg = ctx!(message::Entity::find_by_id(msg_id.clone()).one(db).await)?
        .ok_or_else(|| err_generic!("Unexpected: message doesn't exist"))?;
    let payload = msg
        .payload
        .as_ref()
        .and_then(|value| serde_json::to_string(value).ok())
        .ok_or_else(|| err_generic!("Message payload is NULL"))?;

    span.record("app_id", &msg.app_id.0);
    span.record("org_id", &msg.org_id.0);

    let create_message_app = match CreateMessageApp::layered_fetch(
        cache,
        db,
        None,
        msg.org_id.clone(),
        msg.app_id.clone(),
        Duration::from_secs(30),
    )
    .await?
    {
        Some(create_message_app) => create_message_app,
        None => {
            tracing::info!("Application doesn't exist: {}", &msg.app_id);
            return Ok(());
        }
    };

    let endpoints: Vec<CreateMessageEndpoint> = create_message_app
        .filtered_endpoints(*trigger_type, &msg.event_type, msg.channels.as_ref())
        .iter()
        .filter(|endpoint| match &queue_task {
            QueueTask::HealthCheck => unreachable!(),
            QueueTask::MessageV1(task) => task.endpoint_id == endpoint.id,
            QueueTask::MessageBatch(_) => true,
        })
        .cloned()
        .collect();

    let futures: Vec<_> = match &queue_task {
        QueueTask::HealthCheck => unreachable!(),

        QueueTask::MessageV1(task) => {
            let endpoint = match endpoints.into_iter().next() {
                Some(ep) => ep,
                None => {
                    return Ok(());
                }
            };

            let destination = ctx!(
                messagedestination::Entity::secure_find_by_msg(task.msg_id.clone())
                    .filter(messagedestination::Column::EndpId.eq(endpoint.id.clone()))
                    .one(db)
                    .await
            )?
            .ok_or_else(|| {
                err_generic!(format!(
                    "MessageDestination not found for message {}",
                    &task.msg_id
                ))
            })?;

            vec![dispatch_message_task(
                &worker_context,
                &msg,
                &create_message_app,
                task.clone(),
                &payload,
                endpoint,
                Some(destination),
            )]
        }

        QueueTask::MessageBatch(task) => {
            let destinations: Vec<_> = endpoints
                .iter()
                .map(|endpoint| messagedestination::ActiveModel {
                    msg_id: Set(msg.id.clone()),
                    endp_id: Set(endpoint.id.clone()),
                    next_attempt: Set(Some(Utc::now().into())),
                    status: Set(MessageStatus::Sending),
                    ..Default::default()
                })
                .collect();

            ctx!(
                messagedestination::Entity::insert_many(destinations.clone())
                    .exec(db)
                    .await
            )?;

            endpoints
                .into_iter()
                .zip(destinations)
                .map(|(endpoint, destination)| {
                    let task = MessageTask {
                        msg_id: msg_id.clone(),
                        app_id: task.app_id.clone(),
                        endpoint_id: endpoint.id.clone(),
                        attempt_count: 0,
                        trigger_type: *trigger_type,
                    };

                    dispatch_message_task(
                        &worker_context,
                        &msg,
                        &create_message_app,
                        task,
                        &payload,
                        endpoint,
                        destination.try_into_model().ok(),
                    )
                })
                .collect()
        }
    };

    let join = future::join_all(futures).await;

    let errs: Vec<_> = join.iter().filter(|x| x.is_err()).collect();
    if !errs.is_empty() {
        return Err(err_generic!(format!(
            "Some dispatches failed unexpectedly: {errs:?}"
        )));
    }

    Ok(())
}

lazy_static! {
    pub static ref LAST_QUEUE_POLL: AtomicU64 = get_unix_timestamp().into();
}

async fn update_last_poll_time() {
    LAST_QUEUE_POLL.swap(get_unix_timestamp(), Ordering::Relaxed);
}

/// Listens on the message queue for new tasks
#[allow(clippy::too_many_arguments)]
pub async fn queue_handler(
    cfg: &Configuration,
    cache: Cache,
    db: DatabaseConnection,
    queue_tx: TaskQueueProducer,
    mut queue_rx: TaskQueueConsumer,
    op_webhook_sender: OperationalWebhookSender,
) -> Result<()> {
    static NUM_WORKERS: AtomicUsize = AtomicUsize::new(0);

    let task_limit = cfg.worker_max_tasks;
    if task_limit == 0 {
        tracing::info!("Worker concurrent task limit: unlimited");
    } else {
        tracing::info!("Worker concurrent task limit: {}", task_limit);
    }

    tokio::spawn(
        async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                let num_workers = NUM_WORKERS.load(Ordering::Relaxed);
                if num_workers > 0 {
                    tracing::info!("{} active workers", num_workers);
                }
            }
        }
        .instrument(tracing::error_span!(
            "worker_monitor",
            instance_id = tracing::field::Empty
        )),
    );

    loop {
        if task_limit > 0 {
            let num_workers = NUM_WORKERS.load(Ordering::Relaxed);
            if num_workers > task_limit.into() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        }

        if crate::SHUTTING_DOWN.load(Ordering::SeqCst) {
            tokio::join!(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(500));
                loop {
                    interval.tick().await;
                    let num_workers = NUM_WORKERS.load(Ordering::Relaxed);
                    if num_workers > 0 {
                        tracing::info!(
                            "{} active workers, waiting to shut down worker.",
                            num_workers
                        );
                    } else {
                        tracing::info!("No active workers, shutting down worker.");
                        break;
                    }
                }
            });
            break;
        }

        match queue_rx.receive_all().await {
            Ok(batch) => {
                for delivery in batch {
                    let cfg = cfg.clone();
                    let cache = cache.clone();
                    let db = db.clone();
                    let queue_tx = queue_tx.clone();
                    let queue_task = delivery.task.clone();
                    let op_webhook_sender = op_webhook_sender.clone();

                    tokio::spawn(async move {
                        NUM_WORKERS.fetch_add(1, Ordering::Relaxed);
                        let worker_context = WorkerContext {
                            cfg: &cfg,
                            db: &db,
                            cache: &cache,
                            op_webhook_sender: &op_webhook_sender,
                            queue_tx: &queue_tx,
                        };

                        let queue_task =
                            Arc::try_unwrap(queue_task).unwrap_or_else(|arc| (*arc).clone());
                        if process_queue_task(worker_context, queue_task)
                            .await
                            .is_err()
                        {
                            if let Err(err) = queue_tx.nack(delivery).await {
                                tracing::error!(
                                    "Error sending 'nack' to Redis after task execution error: {}",
                                    err
                                );
                            }
                        } else if let Err(err) = queue_tx.ack(delivery).await {
                            tracing::error!(
                                "Error sending 'ack' to Redis after successful task execution: {}",
                                err
                            );
                        }

                        NUM_WORKERS.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
            Err(err) => {
                tracing::error!("Error receiving task: {:?}", err);
                sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        update_last_poll_time().await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cryptography::AsymmetricKey;
    use crate::core::types::{BaseId, EndpointSecret};

    use bytes::Bytes;
    use ed25519_compact::Signature;
    use std::collections::HashMap;

    // [`generate_msg_headers`] tests
    const TIMESTAMP: i64 = 1;
    const WHITELABEL_HEADERS: bool = false;
    const BODY: &str = "{\"test\": \"body\"}";
    const ENDPOINT_SIGNING_KEYS: &[&EndpointSecretInternal] = &[];
    const ENDPOINT_URL: &str = "http://localhost:8071";

    /// Utility function that returns the default set of headers before configurable header are
    /// accounted for
    fn mock_headers() -> (CaseSensitiveHeaderMap, MessageId) {
        let id = MessageId::new(None, None);

        let signatures = sign_msg(
            &Encryption::new_noop(),
            TIMESTAMP,
            BODY,
            &id,
            ENDPOINT_SIGNING_KEYS,
        );

        (
            generate_msg_headers(
                TIMESTAMP,
                &id,
                signatures,
                WHITELABEL_HEADERS,
                None,
                ENDPOINT_URL,
            )
            .unwrap(),
            id,
        )
    }

    #[test]
    fn test_generate_msg_headers() {
        // The headers to be given to [`generate_msg_headers`]
        let mut headers = HashMap::new();
        headers.insert("test_key".to_owned(), "value".to_owned());

        // The invalid key should be skipped over so it is not included in the expected
        let (mut expected, id) = mock_headers();
        let _ = expected.insert("test_key".to_owned(), "value".parse().unwrap());

        let signatures = sign_msg(
            &Encryption::new_noop(),
            TIMESTAMP,
            BODY,
            &id,
            ENDPOINT_SIGNING_KEYS,
        );

        let actual = generate_msg_headers(
            TIMESTAMP,
            &id,
            signatures,
            WHITELABEL_HEADERS,
            Some(&EndpointHeaders(headers)),
            ENDPOINT_URL,
        )
        .unwrap();

        assert_eq!(expected, actual);
    }

    // Tests endpoint signing keys -- expected values are fetched from the Svix documentation for a
    // direct comparison to the current implementation.
    #[test]
    fn test_generate_msg_headers_with_signing_key() {
        let test_timestamp = 1614265330;
        let test_body = "{\"test\": 2432232314}";
        let test_key = EndpointSecretInternal::from_endpoint_secret(
            EndpointSecret::Symmetric(base64::decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap()),
            &Encryption::new_noop(),
        )
        .unwrap();
        let test_message_id = MessageId("msg_p5jXN8AQM9LWM0D4loKWxJek".to_owned());

        let expected_signature_str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

        let signatures = sign_msg(
            &Encryption::new_noop(),
            test_timestamp,
            test_body,
            &test_message_id,
            &[&test_key],
        );

        let actual = generate_msg_headers(
            test_timestamp,
            &test_message_id,
            signatures,
            WHITELABEL_HEADERS,
            None,
            ENDPOINT_URL,
        )
        .unwrap();

        assert_eq!(
            actual.get("svix-signature").unwrap(),
            expected_signature_str
        );
    }

    // Tests asymmetric signing keys
    #[test]
    fn test_asymmetric_key_signing() {
        let timestamp = 1614265330;
        let body = "{\"test\": 2432232314}";
        let asym_key = AsymmetricKey::from_base64("6Xb/dCcHpPea21PS1N9VY/NZW723CEc77N4rJCubMbfVKIDij2HKpMKkioLlX0dRqSKJp4AJ6p9lMicMFs6Kvg==").unwrap();
        let test_key = EndpointSecretInternal::from_endpoint_secret(
            EndpointSecret::Asymmetric(asym_key.clone()),
            &Encryption::new_noop(),
        )
        .unwrap();
        let msg_id = MessageId("msg_p5jXN8AQM9LWM0D4loKWxJek".to_owned());

        let signatures = sign_msg(
            &Encryption::new_noop(),
            timestamp,
            body,
            &msg_id,
            &[&test_key],
        );

        let to_sign = format!("{msg_id}.{timestamp}.{body}");
        assert!(signatures.starts_with("v1a,"));
        let sig: Signature = Signature::from_slice(
            base64::decode(&signatures["v1a,".len()..])
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        asym_key.0.pk.verify(to_sign.as_bytes(), &sig).unwrap();
        assert_eq!(signatures, "v1a,hnO3f9T8Ytu9HwrXslvumlUpqtNVqkhqw/enGzPCXe5BdqzCInXqYXFymVJaA7AZdpXwVLPo3mNl8EM+m7TBAg==");
    }

    #[test]
    fn test_bytes_to_string() {
        let b = Bytes::from_static(b"Hello, world.");
        assert_eq!(bytes_to_string(b), "Hello, world.");
    }
}
