/*
 * Keeper API
 *
 * No description provided (generated by Openapi Generator https://github.com/openapitools/openapi-generator)
 *
 * The version of the OpenAPI document: 1.0
 * 
 * Generated by: https://openapi-generator.tech
 */




#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InlineObject3 {
    #[serde(rename = "id")]
    pub id: crate::models::ReportId,
    #[serde(rename = "script")]
    pub script: String,
    #[serde(rename = "start_time")]
    pub start_time: String,
}

impl InlineObject3 {
    pub fn new(id: crate::models::ReportId, script: String, start_time: String) -> InlineObject3 {
        InlineObject3 {
            id,
            script,
            start_time,
        }
    }
}

