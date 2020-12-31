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
pub struct ReportOutputBody {
    #[serde(rename = "id")]
    pub id: crate::models::ReportId,
    #[serde(rename = "record")]
    pub record: crate::models::OutputRecord,
}

impl ReportOutputBody {
    pub fn new(id: crate::models::ReportId, record: crate::models::OutputRecord) -> ReportOutputBody {
        ReportOutputBody {
            id,
            record,
        }
    }
}


