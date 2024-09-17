use std::fs;
use std::iter;

use crate::context::*;
use crate::controller::ParserState;
use crate::types::*;
use crate::util::*;
use serde_json::Value;

fn request_dynamic_configuration_from_kakoune(meta: &EditorMeta, ctx: &mut Context) -> Option<()> {
    let fifo = temp_fifo(&meta.session);
    ctx.exec(
        meta.clone(),
        format!("lsp-get-config {}", editor_quote(&fifo.path)),
    );
    let config = std::fs::read_to_string(&fifo.path).unwrap();
    record_dynamic_config(meta, ctx, &config);
    Some(())
}

pub fn request_initialization_options_from_kakoune(
    servers: &[ServerId],
    meta: &EditorMeta,
    ctx: &mut Context,
) -> Vec<Option<Value>> {
    request_dynamic_configuration_from_kakoune(meta, ctx);
    let mut sections = Vec::with_capacity(servers.len());
    for &server_id in servers {
        let server_name = &ctx.server(server_id).name;
        let settings = ctx
            .dynamic_config
            .language_server
            .get(server_name)
            .and_then(|v| v.settings.as_ref());
        let settings = configured_section(meta, ctx, server_id, settings);
        if settings.is_some() {
            sections.push(settings);
            continue;
        }

        let legacy_settings = request_legacy_initialization_options_from_kakoune(meta, ctx);
        if legacy_settings.is_some() {
            sections.push(legacy_settings);
            continue;
        }

        let server_name = &ctx.server(server_id).name;
        let server_config = server_configs(&ctx.config, meta).get(server_name).unwrap();
        let settings = configured_section(meta, ctx, server_id, server_config.settings.as_ref());
        sections.push(settings);
    }
    sections
}

pub fn configured_section(
    meta: &EditorMeta,
    ctx: &Context,
    server_id: ServerId,
    settings: Option<&Value>,
) -> Option<Value> {
    let server_name = &ctx.server(server_id).name;
    settings.and_then(|settings| {
        server_configs(&ctx.config, meta)
            .get(server_name)
            .and_then(|cfg| cfg.settings_section.as_ref())
            .and_then(|section| settings.get(section).cloned())
    })
}

pub fn record_dynamic_config(meta: &EditorMeta, ctx: &mut Context, config: &str) {
    debug!(meta.session, "lsp_config:\n{}", config);
    match toml::from_str(config) {
        Ok(cfg) => {
            ctx.dynamic_config = cfg;
        }
        Err(e) => {
            let msg = format!("failed to parse %opt{{lsp_config}}: {}", e);
            ctx.exec(
                meta.clone(),
                format!("lsp-show-error {}", editor_quote(&msg)),
            );
            panic!("{}", msg)
        }
    };
    if !is_using_legacy_toml(&ctx.config) {
        for (server_name, server) in &meta.language_server {
            let server_id = ctx
                .route_cache
                .get(&(server_name.clone(), server.root.clone()))
                .unwrap();
            ctx.language_servers.get_mut(server_id).unwrap().settings = server.settings.clone();
        }
    }
}

/// User may override initialization options on per-language server basis
/// with `lsp_server_initialization_options` option in Kakoune
/// (i.e. to customize it for specific project).
/// This function asks Kakoune to give such override if any.
pub fn request_legacy_initialization_options_from_kakoune(
    meta: &EditorMeta,
    ctx: &mut Context,
) -> Option<Value> {
    let fifo = temp_fifo(&meta.session);
    ctx.exec(
        meta.clone(),
        format!(
            "lsp-get-server-initialization-options {}",
            editor_quote(&fifo.path)
        ),
    );
    let mut state = ParserState::new(meta.session.clone());
    state.buf = fs::read(fifo.path.clone()).unwrap();
    let server_configuration: Vec<String> = iter::from_fn(|| state.next())
        .take_while(|s| s != "map-end")
        .collect();
    if server_configuration.is_empty() {
        None
    } else {
        Some(Value::Object(explode_str_to_str_map(
            &meta.session,
            &server_configuration,
        )))
    }
}

fn insert_value<'a, 'b, P>(
    target: &'b mut serde_json::Map<String, Value>,
    mut path: P,
    local_key: String,
    value: Value,
) -> Result<(), String>
where
    P: Iterator<Item = &'a str>,
    P: 'a,
{
    match path.next() {
        Some(key) => {
            let maybe_new_target = target
                .entry(key)
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut();

            if maybe_new_target.is_none() {
                return Err(format!(
                    "Expected path {:?} to be object, found {:?}",
                    key, &maybe_new_target,
                ));
            }

            insert_value(maybe_new_target.unwrap(), path, local_key, value)
        }
        None => match target.insert(local_key, value) {
            Some(old_value) => Err(format!("Replaced old value: {:?}", old_value)),
            None => Ok(()),
        },
    }
}
// Take flattened tables like "a.b=1" and produce "{"a":{"b":1}}".
pub fn explode_str_to_str_map(
    session: &SessionId,
    map: &[String],
) -> serde_json::value::Map<String, Value> {
    let mut settings = serde_json::Map::new();

    for map_entry in map.iter() {
        let (raw_key, raw_value) = map_entry.split_once('=').unwrap();
        let mut key_parts = raw_key.split('.');
        let local_key = match key_parts.next_back() {
            Some(name) => name,
            None => {
                warn!(
                    session,
                    "Got a setting with an empty local name: {:?}", raw_key
                );
                continue;
            }
        };
        let toml_value: toml::Value = match toml::from_str(raw_value) {
            Ok(toml_value) => toml_value,
            Err(e) => {
                warn!(
                    session,
                    "Could not parse TOML setting {:?}: {}", raw_value, e
                );
                continue;
            }
        };

        let value: Value = match toml_value.try_into() {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    session,
                    "Could not convert setting {:?} to JSON: {}", raw_value, e
                );
                continue;
            }
        };

        match insert_value(&mut settings, key_parts, local_key.into(), value) {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    session,
                    "Could not set {:?} to {:?}: {}", raw_key, raw_value, e
                );
                continue;
            }
        }
    }

    settings
}
