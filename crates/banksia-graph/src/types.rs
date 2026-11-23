//! The subset of Graph resources Banksia reads and writes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub user_principal_name: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MailFolder {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub parent_folder_id: Option<String>,
    pub child_folder_count: Option<i64>,
    pub total_item_count: Option<i64>,
    pub is_hidden: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EmailAddress {
    pub address: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Recipient {
    pub email_address: Option<EmailAddress>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub change_key: Option<String>,
    pub subject: Option<String>,
    pub from: Option<Recipient>,
    pub received_date_time: Option<String>,
    pub internet_message_id: Option<String>,
    pub has_attachments: Option<bool>,
    pub parent_folder_id: Option<String>,
    /// Present in delta responses for deleted or moved-out messages.
    #[serde(rename = "@removed", default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    pub drive_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemReference {
    pub id: Option<String>,
    pub drive_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub quick_xor_hash: Option<String>,
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    pub mime_type: Option<String>,
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    pub created_date_time: Option<String>,
    pub last_modified_date_time: Option<String>,
    pub parent_reference: Option<ItemReference>,
    pub file: Option<FileFacet>,
    pub folder: Option<Value>,
    pub root: Option<Value>,
    pub deleted: Option<Value>,
    pub package: Option<Value>,
}

impl DriveItem {
    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file
            .as_ref()?
            .hashes
            .as_ref()?
            .quick_xor_hash
            .as_deref()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub upload_url: String,
}
