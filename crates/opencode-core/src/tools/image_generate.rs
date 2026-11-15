//! Image generation and editing tool.

use super::{arg_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct ImageGenerate;

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

#[async_trait]
impl Tool for ImageGenerate {
    fn name(&self) -> &str {
        "experimental_image_generate"
    }
    fn description(&self) -> String {
        [
            "Generates a new image, or edits existing ones.",
            "Only call this when the user asks you to create or edit an image. If you think an image would help but the user didn't ask for one, ask them first instead of generating it.",
            "Images you pass are used as the source to edit, or as references for style and subject.",
            "Each call makes one image. For several images, call it once per image.",
        ].join("\n")
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "minLength": 1, "description": "What to create, or for edits, what to change. Be specific about subject, style, composition and any text that should appear in the image." },
                "images": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "minItems": 1,
                    "maxItems": 8,
                    "description": "Images to edit, or to use as references for style and subject when creating a new image, in order. Local file paths (relative to the session directory or absolute) or http(s) URLs."
                },
                "model": { "type": "string", "description": "NEVER set this unless the user explicitly asks for a particular image model or variant. The value is written as providerID/modelID, or providerID/modelID#variant." },
                "path": { "type": "string", "description": "Where to save the image, relative to the session directory or absolute. The extension is replaced with the image's real format, and an existing file is overwritten. Defaults to a new file in the session's temp folder." }
            },
            "required": ["prompt"]
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let prompt = arg_str(&input, "prompt").ok_or_else(|| anyhow!("prompt is required"))?;
        let providers = ctx.app.providers.read().unwrap().clone();
        let reference = arg_str(&input, "model");
        let resolved = match reference {
            Some(reference) => crate::models::resolve_media(&providers, reference, crate::models::ModelKind::Image)?,
            None => {
                let own = ctx.model.provider_id.clone();
                providers
                    .values()
                    .find(|provider| provider.id == own && !provider.media_models.is_empty())
                    .or_else(|| providers.values().find(|provider| !provider.media_models.is_empty()))
                    .and_then(|provider| {
                        provider.media_models.values().next().map(|model| crate::models::Resolved {
                            provider: provider.clone(),
                            model: model.clone(),
                            variant: None,
                        })
                    })
                    .ok_or_else(|| anyhow!("No image model is available."))?
            }
        };
        let mut images = Vec::new();
        for raw in input.get("images").and_then(Value::as_array).cloned().unwrap_or_default() {
            let Some(value) = raw.as_str() else { continue };
            if value.starts_with("http://") || value.starts_with("https://") {
                let downloaded = crate::provider::media::download(value, "image/png").await?;
                images.push(crate::provider::media::ImageFile { data: downloaded.data, mime: downloaded.mime });
                continue;
            }
            let path = crate::util::resolve(&ctx.directory(), value);
            let data = std::fs::read(&path).map_err(|error| anyhow!("Unable to read image {}: {error}", path.display()))?;
            images.push(crate::provider::media::ImageFile {
                data: base64::engine::general_purpose::STANDARD.encode(data),
                mime: mime_for(&path).to_string(),
            });
        }
        let request = crate::provider::media::ImageRequest {
            prompt: prompt.to_string(),
            images,
            n: Some(1),
            aspect: None,
            resolution: None,
        };
        let mut result = crate::provider::media::images::generate(&resolved.provider, &resolved.model, &request).await?;
        if result.images.is_empty() {
            return Err(anyhow!("The image model returned no images."));
        }
        let requested = arg_str(&input, "path").map(|value| crate::util::resolve(&ctx.directory(), value));
        let mut saved = Vec::new();
        for (index, image) in result.images.drain(..).enumerate() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.data.as_bytes())
                .map_err(|error| anyhow!("Unable to decode generated image: {error}"))?;
            let target = requested.clone().unwrap_or_else(|| {
                let name = if index == 0 { crate::util::id("img") } else { format!("{}-{index}", crate::util::id("img")) };
                crate::paths::get().tmp.join(name)
            });
            let target = PathBuf::from(format!("{}.{}", target.with_extension("").display(), extension(&image.mime)));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, bytes)?;
            saved.push(target);
        }
        let text = saved
            .iter()
            .map(|path| format!("Saved image to {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Output::text(text, format!("Generated {}", saved[0].display()))
            .meta(json!({
                "path": saved[0].display().to_string(),
                "paths": saved.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "model": resolved.model.full_id()
            })))
    }
}
