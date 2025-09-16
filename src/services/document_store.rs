use qdrant_client::qdrant::{PointStruct, UpsertPoints};
use qdrant_client::{Payload, Qdrant};
use embed_anything::embeddings::embed::{EmbeddingResult, Embedder};
use embed_anything::embed_query;
use std::error::Error;

pub struct DocumentStore {
    client: Qdrant,
    embedder: Embedder,
}

impl DocumentStore {
    pub async fn new(qdrant_url: &str) -> Result<Self, Box<dyn Error>> {
        let client = Qdrant::from_url(qdrant_url).build()?;
        let embedder = Embedder::from_pretrained_hf(
            "all-mini-lm-l6-v2",
            "sentence-transformers/all-MiniLM-L6-v2",
            None,
            None,
            None,
        )?;
        Ok(Self { client, embedder })
    }

    pub async fn upsert_tool_result(
        &self,
        record: &crate::components::shared::ToolCallRecord,
    ) -> Result<(), Box<dyn Error>> {
        let collection_name = "tool_results";
        let queries = &[record.result.response.as_str()];
        let embedding_result = embed_query(queries, &self.embedder, None).await?;
        let first_result = embedding_result
            .into_iter()
            .next()
            .ok_or("Embedding result was empty")?;

        let embedding: Vec<f32> = match first_result.embedding {
            EmbeddingResult::DenseVector(vector) => vector.into_iter().map(|f| f as f32).collect(),
            _ => return Err("Unexpected embedding type".into()),
        };
        let value_payload = serde_json::to_value(record)?;
        let payload: Payload = match value_payload {
            serde_json::Value::Object(map) => map.into(),
            _ => Default::default(),
        };

        let points = vec![PointStruct::new(
            uuid::Uuid::new_v4().to_string(),
            embedding,
            payload,
        )];

        let upsert_request = UpsertPoints {
            collection_name: collection_name.to_string(),
            points,
            wait: Some(true),
            ..Default::default()
        };

        self.client.upsert_points(upsert_request).await?;
        Ok(())
    }
}