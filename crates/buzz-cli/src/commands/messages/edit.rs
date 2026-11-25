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
    /// Upload this file and make it the hero.
    File(&'a str),
    /// Put back the hero the message was posted with.
    Original,
}

pub(super) async fn edit_message_with_hero(
    client: &BuzzClient,
    event_id: &str,
    content: &str,
    clear_attachments: bool,
    hero: Option<HeroSource<'_>>,
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
    if hero.is_some() {
        if original.pubkey != client.public_key() {
            return Err(CliError::Usage(
                "only the author of a message can change its hero".into(),
            ));
        }
        // A forum post (45001) can have a message copy, and readers show the copy.
        if original.kind.as_u16() != 9 {
            return Err(CliError::Usage(format!(
                "--event is kind {}; give the article's kind 9 message",
                original.kind.as_u16()
            )));
        }
    }
    // Keep this boundary aligned with media_reference_query and the relay lookup.
    // Other channel-tagged events retain the historical plain-edit behavior.
    if !buzz_core::media::EDIT_GOVERNED_KINDS.contains(&original.kind.as_u16()) {
        if clear_attachments {
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
        vec![serde_json::from_value(snapshot["edit"].clone())
            .map_err(|e| CliError::Other(format!("invalid effective edit: {e}")))?]
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
    let content = if let Some(source) = hero {
        let at = media
            .iter()
            .position(|tag| hero_url(tag).is_some())
            .ok_or_else(|| CliError::Usage("this message has no image to replace".into()))?;
        let old = hero_url(&media[at]).unwrap_or_default().to_owned();
        let tag = match source {
            HeroSource::File(path) => {
                let desc = client.upload_file(path).await?;
                if !desc.mime_type.starts_with("image/") {
                    return Err(CliError::Usage(format!("{path} is not an image")));
                }
                crate::client::build_imeta_tag(&desc)
            }
            HeroSource::Original => original
                .tags
                .iter()
                .map(|tag| tag.as_slice())
                .find(|tag| hero_url(tag).is_some())
                .ok_or_else(|| CliError::Usage("this message was posted without an image".into()))?
                .to_vec(),
        };
        let new = hero_url(&tag)
            .ok_or_else(|| CliError::Other("the relay returned an unusable image".into()))?;
        replacement = replace_image(&effective.content, &old, new);
        validate_content_size(&replacement)?;
        media[at] = tag;
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

/// Point every Markdown image that shows `old` at `new`; links and prose stay.
fn replace_image(content: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(at) = rest.find(old) {
        let (before, after) = rest.split_at(at);
        let after = &after[old.len()..];
        out.push_str(before);
        let line = out.rsplit('\n').next().unwrap_or_default();
        let open = line.strip_suffix('<').unwrap_or(line);
        let image = open
            .strip_suffix("](")
            .is_some_and(|alt| alt.rfind("![").is_some_and(|i| !alt[i..].contains("](")))
            && after.starts_with([')', '>', ' ']);
        out.push_str(if image { new } else { old });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The address of an image attachment; `None` for any other tag.
fn hero_url(tag: &[String]) -> Option<&str> {
    if tag.first().map(String::as_str) != Some("imeta")
        || !tag.iter().any(|part| part.starts_with("m image/"))
    {
        return None;
    }
    tag.iter()
        .find_map(|part| part.strip_prefix("url "))
        .filter(|url| !url.is_empty())
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
