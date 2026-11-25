//! Resolve replacement attachment state before publishing an edit.
use super::{channel_id_from_event, fetch_event};
use crate::{
    client::BuzzClient,
    error::CliError,
    validate::{parse_event_id, validate_content_size, validate_hex64},
};
use nostr::{Event, Timestamp};

#[cfg(test)]
async fn edit_message(
    client: &BuzzClient,
    event_id: &str,
    content: &str,
    clear_attachments: bool,
) -> Result<(), CliError> {
    edit_message_with_hero(client, event_id, content, clear_attachments, None).await
}

pub(super) enum HeroSource<'a> {
    Upload(&'a str),
    Revision(&'a str),
}

pub(super) async fn edit_message_with_hero(
    client: &BuzzClient,
    event_id: &str,
    content: &str,
    clear_attachments: bool,
    hero: Option<(HeroSource<'_>, &str)>,
) -> Result<(), CliError> {
    validate_hex64(event_id)?;
    validate_content_size(content)?;
    let target = parse_event_id(event_id)?;
    let raw = fetch_event(client, &target.to_hex()).await?;
    let channel = channel_id_from_event(event_id, &raw)?;
    let original: Event = serde_json::from_value(raw)
        .map_err(|e| CliError::Other(format!("invalid edit target: {e}")))?;
    if original.id != target {
        return Err(CliError::Other(
            "relay returned the wrong edit target".into(),
        ));
    }
    if hero.is_some() && original.pubkey != client.public_key() {
        return Err(CliError::Usage(
            "Only the original author can replace the hero".into(),
        ));
    }
    // Keep this boundary aligned with media_reference_query and the relay lookup.
    // Other channel-tagged events retain the historical plain-edit behavior.
    if !buzz_core::media::EDIT_GOVERNED_KINDS.contains(&original.kind.as_u16()) {
        if clear_attachments || hero.is_some() {
            return Err(CliError::Usage(format!(
                "attachments on kind {} are not edit-governed",
                original.kind.as_u16()
            )));
        }
        let builder = buzz_sdk::build_edit(channel, target, content)
            .map_err(|e| CliError::Other(format!("build_edit failed: {e}")))?;
        let event = client.sign_event(builder).await?;
        let resp = client.submit_event(event).await?;
        println!("{}", crate::client::normalize_write_response(&resp));
        return Ok(());
    }
    let raw = client
        .query(&serde_json::json!({
            "kinds": buzz_core::media::EDIT_GOVERNED_KINDS, "ids": [target.to_hex()], "#h": [channel.to_string()],
            "effective_edit": 1, "consistency": "strong",
        }))
        .await?;
    let snapshot: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("invalid effective edit response: {e}")))?;
    if snapshot.get("effective_edit") != Some(&serde_json::json!(1))
        || snapshot.get("edit").is_none()
    {
        return Err(CliError::Other(
            "relay does not support effective_edit v1; upgrade the relay before editing".into(),
        ));
    }
    let original: Event = serde_json::from_value(snapshot["original"].clone())
        .map_err(|e| CliError::Other(format!("invalid effective original: {e}")))?;
    if original.id != target
        || !buzz_core::media::EDIT_GOVERNED_KINDS.contains(&original.kind.as_u16())
        || channel_id_from_event(event_id, &snapshot["original"])? != channel
    {
        return Err(CliError::Other(
            "effective edit response has the wrong original".into(),
        ));
    }
    let edits: Vec<Event> = if snapshot["edit"].is_null() {
        vec![]
    } else {
        vec![
            serde_json::from_value(snapshot["edit"].clone())
                .map_err(|e| CliError::Other(format!("invalid effective edit: {e}")))?,
        ]
    };
    let effective = latest_edit(&original, &edits);
    if !edits.is_empty() && effective.id == original.id {
        return Err(CliError::Other(
            "effective edit response has the wrong replacement".into(),
        ));
    }
    let mut media: Vec<Vec<String>> = if clear_attachments {
        Vec::new()
    } else {
        effective
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("imeta"))
            .map(|t| t.as_slice().to_vec())
            .collect()
    };
    let now = Timestamp::now().as_secs();
    let replacement;
    let content = if let Some((source, expected)) = hero {
        validate_hex64(expected)?;
        if effective.id != parse_event_id(expected)? || effective.pubkey != original.pubkey {
            return Err(CliError::Usage(
                "Effective article changed; read it again before editing".into(),
            ));
        }
        let (before, after) = hero_inline(&effective.content, &media)?;
        let tag = match source {
            HeroSource::Upload(path) => {
                let desc = client.upload_file(path).await?;
                if !matches!(
                    desc.mime_type.as_str(),
                    "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                ) {
                    return Err(CliError::Usage(
                        "The replacement must be an inline image".into(),
                    ));
                }
                let mut tag = crate::client::build_imeta_tag(&desc);
                tag.extend(
                    media[0]
                        .iter()
                        .filter(|part| part.starts_with("alt "))
                        .cloned(),
                );
                tag
            }
            HeroSource::Revision(revision) => {
                validate_hex64(revision)?;
                let revision = parse_event_id(revision)?;
                let historical = if revision == original.id {
                    original.clone()
                } else {
                    let raw = fetch_event(client, &revision.to_hex()).await?;
                    serde_json::from_value::<Event>(raw)
                        .map_err(|e| CliError::Other(format!("invalid rollback revision: {e}")))?
                };
                if historical.id != revision
                    || historical.pubkey != original.pubkey
                    || historical.verify().is_err()
                    || historical.id == effective.id
                    || (historical.id != original.id
                        && latest_edit(&original, std::slice::from_ref(&historical)).id != revision)
                    || (historical.created_at, historical.id)
                        >= (effective.created_at, effective.id)
                {
                    return Err(CliError::Usage(
                        "Rollback revision must be an earlier signed revision of this article"
                            .into(),
                    ));
                }
                let first = historical
                    .tags
                    .iter()
                    .find(|tag| tag.as_slice().first().is_some_and(|k| k == "imeta"))
                    .ok_or_else(|| {
                        CliError::Usage("Rollback revision has no hero attachment".into())
                    })?;
                first.as_slice().to_vec()
            }
        };
        let url = hero_url(&tag)?;
        replacement = format!("{before}{url}{after}");
        validate_content_size(&replacement)?;
        media[0] = tag;
        replacement.as_str()
    } else {
        content
    };
    // Make successive edits effective even when issued in the same second.
    // Refuse an unexpectedly future timeline instead of extending it forever.
    let head = effective.created_at.as_secs();
    if head > now.saturating_add(60) {
        return Err(CliError::Other(
            "latest edit is too far in the future; retry when the clock catches up".into(),
        ));
    }
    let created_at = now.max(head + 1);
    let builder = buzz_sdk::build_edit_with_media(channel, target, content, &media)
        .map_err(|e| CliError::Other(format!("build_edit failed: {e}")))?
        .custom_created_at(Timestamp::from(created_at));
    let event = client.sign_event(builder).await?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

/// Only rewrite an unambiguous image at the start of a Markdown line.
fn hero_inline<'a>(
    content: &'a str,
    media: &[Vec<String>],
) -> Result<(&'a str, &'a str), CliError> {
    let invalid =
        || CliError::Usage("The first attachment needs exactly one matching inline image".into());
    let first = media.first().ok_or_else(invalid)?;
    let url = hero_url(first)?;
    let bare = format!("]({url})");
    let angle = format!("](<{url}>)");
    let mut matches = content
        .match_indices(&bare)
        .map(|(i, _)| (i, 2))
        .chain(content.match_indices(&angle).map(|(i, _)| (i, 3)))
        .filter(|(i, _)| {
            let line = content[..*i].rsplit('\n').next().unwrap_or_default();
            line.rfind("![").is_some_and(|start| {
                let prefix = &line[..start];
                !prefix.ends_with('\\')
                    && prefix.trim().is_empty()
                    && !line[start + 2..].contains(']')
            })
        });
    let (index, prefix) = matches.next().ok_or_else(invalid)?;
    if matches.next().is_some() {
        return Err(invalid());
    }
    Ok((
        &content[..index + prefix],
        &content[index + prefix + url.len()..],
    ))
}

fn hero_url(tag: &[String]) -> Result<&str, CliError> {
    let invalid = || CliError::Usage("The hero tag must describe an image with a URL".into());
    if !tag.iter().any(|part| part.starts_with("m image/")) {
        return Err(invalid());
    }
    tag.iter()
        .find_map(|part| part.strip_prefix("url "))
        .filter(|url| !url.is_empty())
        .ok_or_else(invalid)
}

fn latest_edit<'a>(original: &'a Event, edits: &'a [Event]) -> &'a Event {
    let channel = |event: &Event| {
        event
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("h"))
            .and_then(|t| t.content())
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
    };
    edits
        .iter()
        .filter(|edit| {
            edit.kind.as_u16() == 40003
                && channel(edit) == channel(original)
                && buzz_core::media::first_edit_target(edit).as_deref()
                    == Some(original.id.as_bytes().as_slice())
        })
        .max_by_key(|edit| (edit.created_at, edit.id))
        .unwrap_or(original)
}

#[cfg(test)]
mod tests;
