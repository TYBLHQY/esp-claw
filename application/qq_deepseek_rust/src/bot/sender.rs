use super::protocol::{reply_format, Target};
use super::{http, Error, Outbound, OutboundBody, Shared};
use esp_idf_svc::http::Method;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

pub(super) const MAX_NOTIFICATION_BYTES: usize = super::protocol::MAX_NOTIFICATION_BYTES;

/// A proactive message has no borrowed/forged passive msg_id. The API decides permission.
pub(super) fn notify_owner(shared: &Shared, text: &str) -> Result<String, Error> {
    let owner = shared
        .config
        .allow_users
        .first()
        .filter(|_| shared.config.allow_users.len() == 1)
        .ok_or(Error::Config.at("qq_notify_unique_owner_required"))?;
    if text.trim().is_empty() || text.len() > MAX_NOTIFICATION_BYTES {
        return Err(Error::Limit.at("qq_notify_text_limit"));
    }
    if shared.stop.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let body = super::protocol::notification_payload(text)
        .map_err(|_| Error::Limit.at("qq_notify_body_limit"))?;
    let token = shared.tokens.get(&shared.config, &shared.stop)?;
    let authorization = format!("QQBot {token}");
    let uri = format!("{}/v2/users/{owner}/messages", http::QQ_API);
    if shared.stop.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    // Exactly one POST. Transport failure may mean unknown delivery; never replay it here.
    let response = http::json_request(
        Method::Post,
        &uri,
        Some(&authorization),
        Some(&shared.config.qq_app_id),
        &body,
        &shared.stop,
    )?;
    let id = response
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .ok_or(Error::Protocol.at("qq_notify_message_id"))?;
    shared.counters.sent.fetch_add(1, Ordering::Relaxed);
    Ok(id.to_owned())
}

pub(super) fn run(shared: Arc<Shared>, jobs: Receiver<Outbound>) {
    let mut stream: Option<super::stream_sender::Session> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        if stream.as_mut().is_some_and(|s| s.poll(&shared)) {
            stream = None;
        }
        let reply = match jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match reply {
            Outbound {
                context,
                body: OutboundBody::Stream(mailbox),
            } => {
                if stream.is_none() {
                    stream = Some(super::stream_sender::Session::new(context, mailbox));
                } else {
                    mailbox.abandon();
                    shared.error("qq_stream_slot", Error::Limit);
                }
            }
            Outbound {
                context,
                body: OutboundBody::Picture(command),
            } => {
                if !shared.current(&context) {
                    continue;
                }
                if stream.is_some() {
                    let _ = send_reply(
                        &shared,
                        Outbound {
                            context,
                            body: OutboundBody::Text(
                                "正在发送其他流式回复，请等待完成后再发图片。".into(),
                            ),
                        },
                    );
                    continue;
                }
                match prepare_picture(&shared, &context, command) {
                    Ok(Some(session)) => stream = Some(session),
                    Ok(None) => (),
                    Err(Error::Cancelled) => (),
                    Err(error) => {
                        shared.error("qq_picture", error);
                    }
                }
            }
            reply => {
                if let Err(error) = send_reply(&shared, reply) {
                    shared.error("qq_reply", error);
                }
            }
        }
    }
}

pub(super) fn send_reply(shared: &Shared, reply: Outbound) -> Result<(), Error> {
    if !shared.current(&reply.context) {
        return Ok(());
    }
    if matches!(reply.body, OutboundBody::Typing) && !shared.network_headroom() {
        shared
            .delivery
            .typing_skipped
            .fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let token = shared.tokens.get(&shared.config, &shared.stop)?;
    let authorization = format!("QQBot {token}");
    let uri = match &reply.context.target {
        Target::User(id) => format!("{}/v2/users/{id}/messages", http::QQ_API),
        Target::Group(id) => format!("{}/v2/groups/{id}/messages", http::QQ_API),
    };
    let text = match reply.body {
        OutboundBody::Typing => {
            if !matches!(reply.context.target, Target::User(_)) || !shared.network_headroom() {
                shared
                    .delivery
                    .typing_skipped
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let seq = reply
                .context
                .reply_budget
                .reserve()
                .ok_or(Error::Limit.at("qq_typing_quota"))?;
            let body = serde_json::to_vec(&super::protocol::streaming::typing(
                &reply.context.message_id,
                seq,
            ))
            .map_err(|_| Error::Protocol)?;
            // One attempt only; typing responses need not contain a message id.
            let result = http::message_request(
                &uri,
                &authorization,
                &shared.config.qq_app_id,
                &body,
                &shared.stop,
            );
            match result {
                Ok(_) => {
                    shared.delivery.typing_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    shared
                        .delivery
                        .typing_skipped
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            return Ok(());
        }
        OutboundBody::Text(text) => text,
        OutboundBody::Media(media) => {
            let upload_uri = format!("{}/files", uri.trim_end_matches("/messages"));
            let body = serde_json::to_vec(&media.upload_body()).map_err(|_| Error::Protocol)?;
            let uploaded = http::json_request(
                Method::Post,
                &upload_uri,
                Some(&authorization),
                Some(&shared.config.qq_app_id),
                &body,
                &shared.stop,
            );
            let file_info = uploaded.and_then(|response| {
                if response["ttl"].as_u64().unwrap_or(0) == 0 {
                    return Err(Error::Protocol.at("qq_media_ttl"));
                }
                super::protocol::media_reply::file_info(&response)
                    .map(str::to_owned)
                    .ok_or(Error::Protocol.at("qq_media_file_info"))
            });
            match file_info {
                Ok(file_info) => {
                    if !shared.current(&reply.context) {
                        return Ok(());
                    }
                    let seq = reply
                        .context
                        .reply_budget
                        .reserve()
                        .ok_or(Error::Limit.at("qq_reply_quota"))?;
                    let body = serde_json::to_vec(&super::protocol::media_reply::message_body(
                        &file_info,
                        &reply.context.message_id,
                        seq,
                    ))
                    .map_err(|_| Error::Protocol)?;
                    if body.len() > http::BODY_LIMIT {
                        return Err(Error::Limit.at("qq_media_body_limit"));
                    }
                    let response = http::message_request(
                        &uri,
                        &authorization,
                        &shared.config.qq_app_id,
                        &body,
                        &shared.stop,
                    )?;
                    if !response["id"].as_str().is_some_and(|s| !s.is_empty()) {
                        return Err(Error::Protocol.at("qq_media_message_id"));
                    }
                    shared.counters.sent.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(error) => {
                    shared.error("qq_media_upload", error);
                    // srv_send_msg=false: even an unknown upload result cannot have sent media.
                    format!("Media upload did not complete ({}). Check the public URL or QQ account permission.",error.label())
                }
            }
        }
        OutboundBody::Picture(_) => return Err(Error::State.at("qq_picture_owner_required")),
        OutboundBody::Stream(_) => return Err(Error::State.at("qq_stream_actor_required")),
    };
    let mut parts = reply_format::Parts::new(&text);
    let mut markdown = true;
    while let Some(content) = parts.next(reply.context.reply_budget.remaining()) {
        if !shared.current(&reply.context) {
            break;
        }
        let seq = reply
            .context
            .reply_budget
            .reserve()
            .ok_or(Error::Limit.at("qq_reply_quota"))?;
        let body = serde_json::to_vec(&reply_format::payload(
            &content,
            &reply.context.message_id,
            seq,
            markdown,
        ))
        .map_err(|_| Error::Protocol)?;
        let result = http::message_request(
            &uri,
            &authorization,
            &shared.config.qq_app_id,
            &body,
            &shared.stop,
        );
        let response = match result {
            Err(Error::Rejected { status, .. })
                if markdown && reply_format::may_fallback(status) =>
            {
                if !shared.current(&reply.context) {
                    return Ok(());
                }
                markdown = false;
                let seq = reply
                    .context
                    .reply_budget
                    .reserve()
                    .ok_or(Error::Limit.at("qq_fallback_quota"))?;
                let content = if reply.context.reply_budget.remaining() == 0 {
                    parts.finish_here(content)
                } else {
                    content
                };
                let body = serde_json::to_vec(&reply_format::payload(
                    &content,
                    &reply.context.message_id,
                    seq,
                    false,
                ))
                .map_err(|_| Error::Protocol)?;
                http::message_request(
                    &uri,
                    &authorization,
                    &shared.config.qq_app_id,
                    &body,
                    &shared.stop,
                )?
            }
            other => other?,
        };
        // An unconfirmed result never advances to another retry for the same content.
        if !response
            .get("id")
            .and_then(|id| id.as_str())
            .is_some_and(|id| !id.is_empty())
        {
            return Err(Error::Protocol.at("qq_reply_message_id"));
        }
        shared.counters.sent.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn prepare_picture(
    shared: &Shared,
    context: &Arc<super::ReplyContext>,
    command: qq_deepseek_core::material::PictureCommand,
) -> Result<Option<super::stream_sender::Session>, Error> {
    use qq_deepseek_core::material::PictureCommand;
    let prepared = (|| {
        let sd = shared
            .memory_client
            .as_ref()
            .ok_or(Error::State.at("picture_sd_required"))?;
        match command {
            PictureCommand::List => Ok((super::materials::list(sd)?, false)),
            PictureCommand::Send {
                name,
                caption,
                stream,
            } => {
                if stream && !matches!(context.target, Target::User(_)) {
                    return Err(Error::Config.at("picture_private_only"));
                }
                if !shared.network_headroom() {
                    return Err(Error::Limit.at("picture_heap"));
                }
                let prepared = super::materials::prepare(
                    &shared.config,
                    &shared.tokens,
                    sd,
                    &name,
                    &caption,
                    &shared.stop,
                    || shared.current(context),
                )?;
                Ok((prepared.text, stream))
            }
        }
    })();
    let (text, stream) = match prepared {
        Ok(v) => v,
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(error) => {
            // No message POST has occurred; one error notice is safe. Never repeat a failed upload.
            let _ = send_reply(
                shared,
                Outbound {
                    context: context.clone(),
                    body: OutboundBody::Text(format!(
                        "图片未发送（{}）。请检查 SD 文件、图片大小及账号权限。",
                        error.label()
                    )),
                },
            );
            return Err(error);
        }
    };
    if stream {
        let mailbox = shared
            .delivery
            .acquire()
            .ok_or(Error::Limit.at("picture_stream_slot"))?;
        let prefix = "CopilotLee 图文回复\n\n";
        mailbox.preview(prefix);
        return Ok(Some(super::stream_sender::Session::staged(
            context.clone(),
            mailbox,
            format!("{prefix}{text}"),
        )));
    }
    if text.contains("qqbot-media://image/v1/") {
        send_picture_markdown(shared, context, &text)?;
    } else {
        send_reply(
            shared,
            Outbound {
                context: context.clone(),
                body: OutboundBody::Text(text),
            },
        )?;
    }
    Ok(None)
}
pub(super) fn send_picture_markdown(
    shared: &Shared,
    context: &super::ReplyContext,
    text: &str,
) -> Result<(), Error> {
    if !shared.current(context) {
        return Err(Error::Cancelled);
    }
    if text.len() > reply_format::REPLY_BYTES {
        return Err(Error::Limit.at("picture_markdown_limit"));
    }
    let token = shared.tokens.get(&shared.config, &shared.stop)?;
    let uri = match &context.target {
        Target::User(id) => format!("{}/v2/users/{id}/messages", http::QQ_API),
        Target::Group(id) => format!("{}/v2/groups/{id}/messages", http::QQ_API),
    };
    if !shared.current(context) {
        return Err(Error::Cancelled);
    }
    let seq = context
        .reply_budget
        .reserve()
        .ok_or(Error::Limit.at("picture_reply_quota"))?;
    let mut body = reply_format::payload(text, &context.message_id, seq, true);
    body["force_verify_image_resource"] = true.into();
    let body = serde_json::to_vec(&body).map_err(|_| Error::Protocol)?;
    // A single final POST; neither unknown delivery nor explicit rejection replays the image.
    let r = http::message_request(
        &uri,
        &format!("QQBot {token}"),
        &shared.config.qq_app_id,
        &body,
        &shared.stop,
    )?;
    if !r["id"]
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
    {
        return Err(Error::Protocol.at("picture_message_id"));
    }
    shared.counters.sent.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
