use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{debug_handler, Json};
use itertools::izip;
use metrics::counter;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::iter::repeat;
use tracing::instrument;
use uuid::Uuid;

use crate::clickhouse::ClickHouseConnectionInfo;
use crate::error::{Error, ErrorDetails};
use crate::function::sample_variant;
use crate::gateway_util::{AppState, AppStateData, StructuredJson};
use crate::inference::types::{
    batch::BatchModelInferenceWithMetadata, ContentBlockOutput, Input, JsonInferenceOutput, Usage,
};
use crate::tool::{
    BatchDynamicToolParams, BatchDynamicToolParamsWithSize, DynamicToolParams,
    ToolCallConfigDatabaseInsert,
};
use crate::uuid_util::validate_episode_id;
use crate::variant::{BatchInferenceConfig, Variant};

use super::inference::{
    ChatCompletionInferenceParams, InferenceClients, InferenceModels, InferenceParams,
};

/// The expected payload is a JSON object with the following fields:
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    // the function name
    pub function_name: String,
    // the episode IDs for each inference (if not provided, it'll be set to inference_id)
    // NOTE: DO NOT GENERATE EPISODE IDS MANUALLY. THE API WILL DO THAT FOR YOU.
    #[serde(default)]
    pub episode_ids: Option<BatchEpisodeIdInput>,
    // the inputs for the inferences
    pub inputs: Vec<Input>,
    // Inference-time overrides for variant types (use with caution)
    #[serde(default)]
    pub params: BatchInferenceParams,
    // if the client would like to pin a specific variant to be used
    // NOTE: YOU SHOULD TYPICALLY LET THE API SELECT A VARIANT FOR YOU (I.E. IGNORE THIS FIELD).
    //       ONLY PIN A VARIANT FOR SPECIAL USE CASES (E.G. TESTING / DEBUGGING VARIANTS).
    pub variant_name: Option<String>,
    // the tags to add to the inference
    #[serde(default)]
    pub tags: Option<BatchTags>,
    // dynamic information about tool calling. Don't directly include `dynamic_tool_params` in `Params`.
    #[serde(flatten)]
    pub dynamic_tool_params: BatchDynamicToolParams,
    // `dynamic_tool_params` includes the following fields, passed at the top level of `Params`:
    // If provided, the inference will only use the specified tools (a subset of the function's tools)
    // allowed_tools: Option<Vec<Option<Vec<String>>>>,
    // If provided, the inference will use the specified tools in addition to the function's tools
    // additional_tools: Option<Vec<Option<Vec<Tool>>>>,
    // If provided, the inference will use the specified tool choice
    // tool_choice: Option<Vec<Option<ToolChoice>>>,
    // If true, the inference will use parallel tool calls
    // parallel_tool_calls: Option<Vec<Option<bool>>>,
    // If provided for a JSON inference, the inference will use the specified output schema instead of the
    // configured one. We only lazily validate this schema.
    #[serde(default)]
    pub output_schemas: Option<BatchOutputSchemas>,
    #[serde(default)]
    pub credentials: InferenceCredentials,
}

type BatchEpisodeIdInput = Vec<Option<Uuid>>;
type BatchEpisodeIds = Vec<Uuid>;
type BatchTags = Vec<Option<HashMap<String, String>>>;
type BatchOutputSchemas = Vec<Option<Value>>;

pub type InferenceCredentials = HashMap<String, SecretString>;

/// A handler for the inference endpoint
#[instrument(
    name="post_batch_inference",
    skip(config, http_client, clickhouse_connection_info, params),
    fields(
        function_name = %params.function_name,
        variant_name = ?params.variant_name,
    )
)]
#[debug_handler(state = AppStateData)]
pub async fn prepare_batch_inference_handler(
    State(AppStateData {
        config,
        http_client,
        clickhouse_connection_info,
    }): AppState,
    StructuredJson(params): StructuredJson<Params>,
) -> Result<Response<Body>, Error> {
    // Get the function config or return an error if it doesn't exist
    let function = config.get_function(&params.function_name)?;
    let num_inferences = params.inputs.len();
    if num_inferences == 0 {
        return Err(ErrorDetails::InvalidRequest {
            message: "No inputs provided".to_string(),
        }
        .into());
    }
    let batch_dynamic_tool_params: Vec<DynamicToolParams> =
        BatchDynamicToolParamsWithSize(params.dynamic_tool_params, num_inferences).try_into()?;

    let tool_configs = batch_dynamic_tool_params
        .into_iter()
        .map(|dynamic_tool_params| function.prepare_tool_config(dynamic_tool_params, &config.tools))
        .collect::<Result<Vec<_>, _>>()?;
    // Collect the function variant names as a Vec<&str>
    let mut candidate_variant_names: Vec<&str> =
        function.variants().keys().map(AsRef::as_ref).collect();

    // If the function has no variants, return an error
    if candidate_variant_names.is_empty() {
        return Err(ErrorDetails::InvalidFunctionVariants {
            message: format!("Function `{}` has no variants", params.function_name),
        }
        .into());
    }

    // Validate the input
    params
        .inputs
        .iter()
        .enumerate()
        .try_for_each(|(i, input)| {
            function.validate_input(input).map_err(|e| {
                Error::new(ErrorDetails::BatchInputValidation {
                    index: i,
                    message: e.to_string(),
                })
            })
        })?;

    // If a variant is pinned, only that variant should be attempted
    if let Some(ref variant_name) = params.variant_name {
        candidate_variant_names.retain(|k| k == variant_name);

        // If the pinned variant doesn't exist, return an error
        if candidate_variant_names.is_empty() {
            return Err(ErrorDetails::UnknownVariant {
                name: variant_name.to_string(),
            }
            .into());
        }
    }

    // Retrieve or generate the episode IDs and validate them (in the impl)
    let episode_ids: BatchEpisodeIds =
        BatchEpisodeIdsWithSize(params.episode_ids, num_inferences).try_into()?;

    // Increment the request count
    counter!(
        "request_count",
        "endpoint" => "post_batch_inference",
        "function_name" => params.function_name.to_string(),
    )
    .increment(1);
    counter!(
        "inference_count",
        "endpoint" => "post_batch_inference",
        "function_name" => params.function_name.to_string(),
    )
    .increment(num_inferences as u64);

    // Keep track of which variants failed
    let mut variant_errors = std::collections::HashMap::new();
    let inference_config =
        BatchInferenceConfig::new(&config.templates, tool_configs, params.output_schemas);

    let inference_clients = InferenceClients {
        http_client: &http_client,
        clickhouse_connection_info: &clickhouse_connection_info,
        credentials: &params.credentials,
    };

    let inference_models = InferenceModels {
        models: &config.models,
        embedding_models: &config.embedding_models,
    };
    let inference_params: Vec<InferenceParams> =
        BatchInferenceParamsWithSize(params.params, num_inferences).try_into()?;

    // Keep sampling variants until one succeeds
    // We already guarantee there is at least one inference
    let first_episode_id = episode_ids
        .first()
        .ok_or_else(|| Error::new(ErrorDetails::Inference {
            message: "batch episode_ids unexpectedly empty. This should never happen. Please file a bug report: https://github.com/tensorzero/tensorzero/issues/new".to_string(),
        }))?;
    let inference_configs = inference_config.inference_configs();
    while !candidate_variant_names.is_empty() {
        // We sample the same variant for the whole batch
        let (variant_name, variant) = sample_variant(
            &mut candidate_variant_names,
            function.variants(),
            &params.function_name,
            first_episode_id,
        )?;
        // Will be edited by the variant as part of making the request so we must clone here
        let variant_inference_params = inference_params.clone();

        let result = variant
            .start_batch_inference(
                &params.inputs,
                &inference_models,
                function,
                &inference_configs,
                &inference_clients,
                variant_inference_params,
            )
            .await;

        let result = match result {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                        "functions.{function_name}.variants.{variant_name} failed during inference: {e}",
                        function_name = params.function_name,
                        variant_name = variant_name,
                    );
                variant_errors.insert(variant_name.to_string(), e);
                continue;
            }
        };

        // Write to ClickHouse (don't spawn a thread for this because it's required)
        let write_metadata = BatchInferenceDatabaseInsertMetadata {
            function_name: params.function_name.as_str(),
            variant_name,
            episode_ids: &episode_ids,
            tags: params.tags,
        };

        let (batch_id, inference_ids) = write_inference(
            &clickhouse_connection_info,
            params.inputs,
            result,
            write_metadata,
            inference_config.clone(),
            // Spent a while fighting the borrow checker here, gave up
            // The issue is that inference_config holds the ToolConfigs and ModelInferenceRequest has lifetimes that conflict with the inference_config
        )
        .await?;

        return Ok(Json(PrepareBatchInferenceOutput {
            batch_id,
            inference_ids,
            episode_ids,
        })
        .into_response());
    }

    // Eventually, if we get here, it means we tried every variant and none of them worked
    Err(ErrorDetails::AllVariantsFailed {
        errors: variant_errors,
    }
    .into())
}

#[derive(Debug, Serialize)]
struct PrepareBatchInferenceOutput {
    batch_id: Uuid,
    inference_ids: Vec<Uuid>,
    episode_ids: Vec<Uuid>,
}

#[derive(Debug)]
struct BatchInferenceDatabaseInsertMetadata<'a> {
    pub function_name: &'a str,
    pub variant_name: &'a str,
    pub episode_ids: &'a Vec<Uuid>,
    pub tags: Option<Vec<Option<HashMap<String, String>>>>,
    // pub tool_configs: &'a Vec<Option<ToolCallConfig>>,
}

#[derive(Debug, Serialize)]
struct BatchModelInferenceInsert<'a> {
    pub id: String,
    pub batch_id: &'a str,
    pub function_name: &'a str,
    pub variant_name: &'a str,
    pub episode_id: String,
    pub input: String,
    pub input_messages: String,
    pub system: Option<String>,
    pub tool_params: Option<ToolCallConfigDatabaseInsert>,
    pub inference_params: &'a InferenceParams,
    pub output_schema: Option<String>,
    pub model_name: &'a str,
    pub model_provider_name: &'a str,
    pub tags: Option<HashMap<String, String>>,
}

// Returns the batch ID and the inference IDs that were written to ClickHouse
async fn write_inference<'a>(
    clickhouse_connection_info: &ClickHouseConnectionInfo,
    inputs: Vec<Input>,
    result: BatchModelInferenceWithMetadata<'a>,
    metadata: BatchInferenceDatabaseInsertMetadata<'a>,
    inference_config: BatchInferenceConfig<'a>,
) -> Result<(Uuid, Vec<Uuid>), Error> {
    let mut rows = vec![];
    let batch_id = result.batch_id.to_string();

    for (
        i,
        inference_id,
        input,
        input_messages,
        system,
        tool_config,
        inference_params,
        output_schema,
        tags,
    ) in izip!(
        0..,
        result.inference_ids.iter(),
        inputs,
        result.input_messages.iter(),
        result.systems.iter(),
        inference_config.tool_configs,
        result.inference_params.iter(),
        result.output_schemas.iter(),
        metadata
            .tags
            .unwrap_or_default()
            .into_iter()
            .chain(repeat(None)),
    ) {
        let input = serde_json::to_string(&input).map_err(|e| {
            Error::new(ErrorDetails::Serialization {
                message: e.to_string(),
            })
        })?;
        let input_messages = serde_json::to_string(&input_messages).map_err(|e| {
            Error::new(ErrorDetails::Serialization {
                message: e.to_string(),
            })
        })?;
        let system = system
            .as_ref()
            .map(|s| serde_json::to_string(&s))
            .transpose()
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: e.to_string(),
                })
            })?;
        let tool_params: Option<ToolCallConfigDatabaseInsert> = tool_config.map(|t| t.into());
        let output_schema = output_schema
            .map(|s| serde_json::to_string(&s))
            .transpose()
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: e.to_string(),
                })
            })?;
        rows.push(BatchModelInferenceInsert {
            id: inference_id.to_string(),
            batch_id: &batch_id,
            function_name: metadata.function_name,
            variant_name: metadata.variant_name,
            episode_id: metadata.episode_ids[i].to_string(),
            input,
            input_messages,
            system,
            tool_params,
            inference_params,
            output_schema,
            model_name: result.model_name,
            model_provider_name: result.model_provider_name,
            tags,
        });
    }
    clickhouse_connection_info
        .write(&rows, "BatchModelInference")
        .await?;
    Ok((result.batch_id, result.inference_ids))
}

/// InferenceResponse and InferenceResultChunk determine what gets serialized and sent to the client

#[derive(Clone, Debug, Serialize)]
#[serde(untagged, rename_all = "snake_case")]
pub enum InferenceResponse {
    Chat(ChatInferenceResponse),
    Json(JsonInferenceResponse),
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatInferenceResponse {
    pub inference_id: Uuid,
    pub episode_id: Uuid,
    pub variant_name: String,
    pub content: Vec<ContentBlockOutput>,
    pub usage: Usage,
}

#[derive(Clone, Debug, Serialize)]
pub struct JsonInferenceResponse {
    pub inference_id: Uuid,
    pub episode_id: Uuid,
    pub variant_name: String,
    pub output: JsonInferenceOutput,
    pub usage: Usage,
}

struct BatchEpisodeIdsWithSize(Option<BatchEpisodeIdInput>, usize);

impl TryFrom<BatchEpisodeIdsWithSize> for BatchEpisodeIds {
    type Error = Error;

    fn try_from(
        BatchEpisodeIdsWithSize(episode_ids, num_inferences): BatchEpisodeIdsWithSize,
    ) -> Result<Self, Self::Error> {
        let episode_ids = match episode_ids {
            Some(episode_ids) => {
                if episode_ids.len() != num_inferences {
                    return Err(ErrorDetails::InvalidRequest {
                        message: format!(
                            "Number of episode_ids ({}) does not match number of inputs ({})",
                            episode_ids.len(),
                            num_inferences
                        ),
                    }
                    .into());
                }

                episode_ids
                    .into_iter()
                    .map(|id| id.unwrap_or_else(Uuid::now_v7))
                    .collect()
            }
            None => vec![Uuid::now_v7(); num_inferences],
        };
        episode_ids.iter().enumerate().try_for_each(|(i, id)| {
            validate_episode_id(*id).map_err(|e| {
                Error::new(ErrorDetails::BatchInputValidation {
                    index: i,
                    message: e.to_string(),
                })
            })
        })?;
        Ok(episode_ids)
    }
}

/// InferenceParams is the top-level struct for inference parameters.
/// We backfill these from the configs given in the variants used and ultimately write them to the database.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct BatchInferenceParams {
    pub chat_completion: BatchChatCompletionInferenceParams,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct BatchChatCompletionInferenceParams {
    #[serde(default)]
    pub temperature: Option<Vec<Option<f32>>>,
    #[serde(default)]
    pub max_tokens: Option<Vec<Option<u32>>>,
    #[serde(default)]
    pub seed: Option<Vec<Option<u32>>>,
    #[serde(default)]
    pub top_p: Option<Vec<Option<f32>>>,
    #[serde(default)]
    pub presence_penalty: Option<Vec<Option<f32>>>,
    #[serde(default)]
    pub frequency_penalty: Option<Vec<Option<f32>>>,
}

struct BatchInferenceParamsWithSize(BatchInferenceParams, usize);
impl TryFrom<BatchInferenceParamsWithSize> for Vec<InferenceParams> {
    type Error = Error;

    fn try_from(
        BatchInferenceParamsWithSize(params, num_inferences): BatchInferenceParamsWithSize,
    ) -> Result<Self, Self::Error> {
        let BatchInferenceParams { chat_completion } = params;
        let chat_completion_params: Vec<ChatCompletionInferenceParams> =
            BatchChatCompletionParamsWithSize(chat_completion, num_inferences).try_into()?;
        Ok(chat_completion_params
            .into_iter()
            .map(|p| InferenceParams { chat_completion: p })
            .collect())
    }
}

struct BatchChatCompletionParamsWithSize(BatchChatCompletionInferenceParams, usize);
impl TryFrom<BatchChatCompletionParamsWithSize> for Vec<ChatCompletionInferenceParams> {
    type Error = Error;

    fn try_from(
        BatchChatCompletionParamsWithSize(params, num_inferences): BatchChatCompletionParamsWithSize,
    ) -> Result<Self, Self::Error> {
        let BatchChatCompletionInferenceParams {
            temperature,
            max_tokens,
            seed,
            top_p,
            presence_penalty,
            frequency_penalty,
        } = params;
        // Verify all provided Vecs have the same length
        if let Some(temperature) = &temperature {
            if temperature.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "temperature vector length ({}) does not match number of inferences ({})",
                        temperature.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        if let Some(max_tokens) = &max_tokens {
            if max_tokens.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "max_tokens vector length ({}) does not match number of inferences ({})",
                        max_tokens.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        if let Some(seed) = &seed {
            if seed.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "seed vector length ({}) does not match number of inferences ({})",
                        seed.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        if let Some(top_p) = &top_p {
            if top_p.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "top_p vector length ({}) does not match number of inferences ({})",
                        top_p.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        if let Some(presence_penalty) = &presence_penalty {
            if presence_penalty.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "presence_penalty vector length ({}) does not match number of inferences ({})",
                        presence_penalty.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        if let Some(frequency_penalty) = &frequency_penalty {
            if frequency_penalty.len() != num_inferences {
                return Err(ErrorDetails::InvalidRequest {
                    message: format!(
                        "frequency_penalty vector length ({}) does not match number of inferences ({})",
                        frequency_penalty.len(),
                        num_inferences
                    ),
                }
                .into());
            }
        }

        // Convert Option<Vec<Option<T>>> into Vec<Option<T>> by unwrapping or creating empty vec
        let temperature = temperature.unwrap_or_default();
        let max_tokens = max_tokens.unwrap_or_default();
        let seed = seed.unwrap_or_default();
        let top_p = top_p.unwrap_or_default();
        let presence_penalty = presence_penalty.unwrap_or_default();
        let frequency_penalty = frequency_penalty.unwrap_or_default();

        // Create iterators that take ownership
        let mut temperature_iter = temperature.into_iter();
        let mut max_tokens_iter = max_tokens.into_iter();
        let mut seed_iter = seed.into_iter();
        let mut top_p_iter = top_p.into_iter();
        let mut presence_penalty_iter = presence_penalty.into_iter();
        let mut frequency_penalty_iter = frequency_penalty.into_iter();

        // Build params using the iterators
        let mut all_inference_params = Vec::with_capacity(num_inferences);
        for _ in 0..num_inferences {
            all_inference_params.push(ChatCompletionInferenceParams {
                temperature: temperature_iter.next().unwrap_or(None),
                max_tokens: max_tokens_iter.next().unwrap_or(None),
                seed: seed_iter.next().unwrap_or(None),
                top_p: top_p_iter.next().unwrap_or(None),
                presence_penalty: presence_penalty_iter.next().unwrap_or(None),
                frequency_penalty: frequency_penalty_iter.next().unwrap_or(None),
            });
        }
        Ok(all_inference_params)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Timestamp;

    use super::*;

    #[test]
    fn test_try_from_batch_episode_ids_with_size() {
        let batch_episode_ids_with_size = BatchEpisodeIdsWithSize(None, 3);
        let batch_episode_ids = BatchEpisodeIds::try_from(batch_episode_ids_with_size).unwrap();
        assert_eq!(batch_episode_ids.len(), 3);

        let batch_episode_ids_with_size = BatchEpisodeIdsWithSize(Some(vec![None, None, None]), 3);
        let batch_episode_ids = BatchEpisodeIds::try_from(batch_episode_ids_with_size).unwrap();
        assert_eq!(batch_episode_ids.len(), 3);

        let episode_id_0 = Uuid::now_v7();
        let episode_id_1 = Uuid::now_v7();
        let batch_episode_ids_with_size =
            BatchEpisodeIdsWithSize(Some(vec![Some(episode_id_0), Some(episode_id_1), None]), 3);
        let batch_episode_ids = BatchEpisodeIds::try_from(batch_episode_ids_with_size).unwrap();
        assert_eq!(batch_episode_ids.len(), 3);
        assert_eq!(batch_episode_ids[0], episode_id_0);
        assert_eq!(batch_episode_ids[1], episode_id_1);

        let early_uuid = Uuid::new_v7(Timestamp::from_unix_time(946766218, 0, 0, 0));
        let batch_episode_ids_with_size =
            BatchEpisodeIdsWithSize(Some(vec![Some(early_uuid), None, None]), 3);
        let err = BatchEpisodeIds::try_from(batch_episode_ids_with_size).unwrap_err();
        assert_eq!(
            err,
            ErrorDetails::BatchInputValidation {
                index: 0,
                message: "Invalid Episode ID: Timestamp is too early".to_string(),
            }
            .into()
        );
    }

    #[test]
    fn test_batch_inference_params_with_size() {
        // Try with default params
        let batch_inference_params_with_size =
            BatchInferenceParamsWithSize(BatchInferenceParams::default(), 3);
        let inference_params =
            Vec::<InferenceParams>::try_from(batch_inference_params_with_size).unwrap();
        assert_eq!(inference_params.len(), 3);
        assert_eq!(
            inference_params[0].chat_completion,
            ChatCompletionInferenceParams::default()
        );

        // Try with some overridden params
        let batch_inference_params_with_size = BatchInferenceParamsWithSize(
            BatchInferenceParams {
                chat_completion: BatchChatCompletionInferenceParams {
                    temperature: Some(vec![Some(0.5), None, None]),
                    max_tokens: Some(vec![None, None, Some(30)]),
                    seed: Some(vec![None, Some(2), Some(3)]),
                    top_p: None,
                    presence_penalty: Some(vec![Some(0.5), Some(0.6), Some(0.7)]),
                    frequency_penalty: Some(vec![Some(0.5), Some(0.6), Some(0.7)]),
                },
            },
            3,
        );

        let inference_params =
            Vec::<InferenceParams>::try_from(batch_inference_params_with_size).unwrap();
        assert_eq!(inference_params.len(), 3);
        assert_eq!(inference_params[0].chat_completion.temperature, Some(0.5));
        assert_eq!(inference_params[1].chat_completion.max_tokens, None);
        assert_eq!(inference_params[2].chat_completion.seed, Some(3));
        // Check top_p is None for all since it wasn't specified
        assert_eq!(inference_params[0].chat_completion.top_p, None);
        assert_eq!(inference_params[1].chat_completion.top_p, None);
        assert_eq!(inference_params[2].chat_completion.top_p, None);

        // Check presence_penalty values
        assert_eq!(
            inference_params[0].chat_completion.presence_penalty,
            Some(0.5)
        );
        assert_eq!(
            inference_params[1].chat_completion.presence_penalty,
            Some(0.6)
        );
        assert_eq!(
            inference_params[2].chat_completion.presence_penalty,
            Some(0.7)
        );

        // Check frequency_penalty values
        assert_eq!(
            inference_params[0].chat_completion.frequency_penalty,
            Some(0.5)
        );
        assert_eq!(
            inference_params[1].chat_completion.frequency_penalty,
            Some(0.6)
        );
        assert_eq!(
            inference_params[2].chat_completion.frequency_penalty,
            Some(0.7)
        );

        // Verify temperature is None for indices 1 and 2
        assert_eq!(inference_params[1].chat_completion.temperature, None);
        assert_eq!(inference_params[2].chat_completion.temperature, None);

        // Verify max_tokens is 30 for last item and None for first
        assert_eq!(inference_params[0].chat_completion.max_tokens, None);
        assert_eq!(inference_params[2].chat_completion.max_tokens, Some(30));

        // Verify seed is None for first item and 2 for second
        assert_eq!(inference_params[0].chat_completion.seed, None);
        assert_eq!(inference_params[1].chat_completion.seed, Some(2));

        // Test with ragged arrays (arrays of different lengths)
        let batch_inference_params_with_size = BatchInferenceParamsWithSize(
            BatchInferenceParams {
                chat_completion: BatchChatCompletionInferenceParams {
                    temperature: Some(vec![Some(0.5), None]), // Too short
                    max_tokens: Some(vec![None, None, Some(30), Some(40)]), // Too long
                    seed: Some(vec![]),                       // Empty array
                    top_p: None,
                    presence_penalty: Some(vec![Some(0.5)]), // Too short
                    frequency_penalty: Some(vec![Some(0.5), Some(0.6), Some(0.7), Some(0.8)]), // Too long
                },
            },
            3,
        );

        let err = Vec::<InferenceParams>::try_from(batch_inference_params_with_size).unwrap_err();
        match err.get_details() {
            ErrorDetails::InvalidRequest { message } => assert_eq!(
                message,
                "temperature vector length (2) does not match number of inferences (3)"
            ),
            _ => panic!("Expected InvalidRequest error"),
        }

        // Test with wrong size specified
        let batch_inference_params_with_size = BatchInferenceParamsWithSize(
            BatchInferenceParams {
                chat_completion: BatchChatCompletionInferenceParams {
                    temperature: Some(vec![Some(0.5), None, None, None]),
                    max_tokens: Some(vec![None, None, Some(30)]),
                    seed: Some(vec![None, Some(2), Some(3)]),
                    top_p: None,
                    presence_penalty: Some(vec![Some(0.5), Some(0.6), Some(0.7)]),
                    frequency_penalty: Some(vec![Some(0.5), Some(0.6), Some(0.7)]),
                },
            },
            4, // Wrong size - arrays are length 3 but size is 4
        );

        let err = Vec::<InferenceParams>::try_from(batch_inference_params_with_size).unwrap_err();
        match err.get_details() {
            ErrorDetails::InvalidRequest { message } => assert_eq!(
                message,
                "max_tokens vector length (3) does not match number of inferences (4)"
            ),
            _ => panic!("Expected InvalidRequest error"),
        }
    }
}