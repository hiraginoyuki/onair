use axum::Json;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ModelMeta>,
}

#[derive(Debug, Serialize)]
pub struct ModelMeta {
    pub n_ctx: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n_ctx_train: Option<u64>,
}

impl ModelObject {
    pub fn new(id: String, context_length: Option<u64>) -> Self {
        Self {
            id,
            object: "model",
            created: 0,
            owned_by: "onair",
            meta: context_length.map(|n_ctx| ModelMeta {
                n_ctx,
                n_ctx_train: None,
            }),
        }
    }

    pub fn new_static(id: String, context_length: u64) -> Self {
        Self {
            id,
            object: "model",
            created: 0,
            owned_by: "onair",
            meta: Some(ModelMeta {
                n_ctx: context_length,
                n_ctx_train: Some(context_length),
            }),
        }
    }
}

pub fn models_response(models: impl IntoIterator<Item = ModelObject>) -> Json<ModelsResponse> {
    let mut data = models.into_iter().collect::<Vec<_>>();
    data.sort_by(|left, right| left.id.cmp(&right.id));
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

pub fn model_response(model: String, context_length: Option<u64>) -> Json<ModelObject> {
    Json(ModelObject::new(model, context_length))
}

pub fn model_response_with_n_ctx_train(model: String, n_ctx: u64) -> Json<ModelObject> {
    Json(ModelObject::new_static(model, n_ctx))
}

#[derive(Debug, Serialize)]
pub struct PropsResponse {
    pub default_generation_settings: DefaultGenerationSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    pub model_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct DefaultGenerationSettings {
    pub params: Value,
    pub n_ctx: u64,
}

pub fn props_response(
    role: Option<&'static str>,
    model_alias: Option<String>,
    n_ctx: u64,
) -> Json<PropsResponse> {
    Json(PropsResponse {
        default_generation_settings: DefaultGenerationSettings {
            params: json!({}),
            n_ctx,
        },
        model_alias,
        model_path: "none".to_owned(),
        role,
    })
}
