// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    common::{
        Batch, Context, ContextFromDb, KeyIterable, KeyValueIterable, KeyValueStoreClient,
        WriteOperation,
    },
    localstack,
};
use async_trait::async_trait;
use aws_sdk_dynamodb::{
    model::{
        AttributeDefinition, AttributeValue, DeleteRequest, KeySchemaElement, KeyType,
        ProvisionedThroughput, PutRequest, ScalarAttributeType, WriteRequest,
    },
    output::QueryOutput,
    types::{Blob, SdkError},
    Client,
};
use serde::Serialize;
use std::{collections::HashMap, str::FromStr};
use thiserror::Error;

/// The configuration to connect to DynamoDB.
pub type Config = aws_sdk_dynamodb::Config;

#[cfg(test)]
#[path = "unit_tests/dynamo_db_context_tests.rs"]
mod dynamo_db_context_tests;

/// The attribute name of the partition key.
const PARTITION_ATTRIBUTE: &str = "item_partition";

/// A dummy value to use as the partition key.
const DUMMY_PARTITION_KEY: &[u8] = &[0];

/// The attribute name of the primary key (used as a sort key).
const KEY_ATTRIBUTE: &str = "item_key";

/// The attribute name of the table value blob.
const VALUE_ATTRIBUTE: &str = "item_value";

/// The attribute for obtaining the primary key (used as a sort key) with the stored value.
const KEY_VALUE_ATTRIBUTE: &str = "item_key, item_value";

/// A DynamoDb client.
#[derive(Debug, Clone)]
pub struct DynamoDbClient {
    client: Client,
    table: TableName,
}

/// A implementation of [`Context`] based on [`DynamoDbClient`].
pub type DynamoDbContext<E> = ContextFromDb<E, DynamoDbClient>;

impl DynamoDbClient {
    /// Build the key attributes for a table item.
    ///
    /// The key is composed of two attributes that are both binary blobs. The first attribute is a
    /// partition key and is currently just a dummy value that ensures all items are in the same
    /// partion. This is necessary for range queries to work correctly.
    ///
    /// The second attribute is the actual key value, which is generated by concatenating the
    /// context prefix. The Vec<u8> expression is obtained from self.derive_key
    fn build_key(key: Vec<u8>) -> HashMap<String, AttributeValue> {
        [
            (
                PARTITION_ATTRIBUTE.to_owned(),
                AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
            ),
            (KEY_ATTRIBUTE.to_owned(), AttributeValue::B(Blob::new(key))),
        ]
        .into()
    }

    /// Build the value attribute for storing a table item.
    fn build_key_value(key: Vec<u8>, value: Vec<u8>) -> HashMap<String, AttributeValue> {
        [
            (
                PARTITION_ATTRIBUTE.to_owned(),
                AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
            ),
            (KEY_ATTRIBUTE.to_owned(), AttributeValue::B(Blob::new(key))),
            (
                VALUE_ATTRIBUTE.to_owned(),
                AttributeValue::B(Blob::new(value)),
            ),
        ]
        .into()
    }

    /// Extract the key attribute from an item.
    fn extract_key(
        prefix_len: usize,
        attributes: &HashMap<String, AttributeValue>,
    ) -> Result<&[u8], DynamoDbContextError> {
        let key = attributes
            .get(KEY_ATTRIBUTE)
            .ok_or(DynamoDbContextError::MissingKey)?;
        match key {
            AttributeValue::B(blob) => Ok(&blob.as_ref()[prefix_len..]),
            key => Err(DynamoDbContextError::wrong_key_type(key)),
        }
    }

    /// Extract the value attribute from an item.
    fn extract_value(
        attributes: &HashMap<String, AttributeValue>,
    ) -> Result<&[u8], DynamoDbContextError> {
        let value = attributes
            .get(VALUE_ATTRIBUTE)
            .ok_or(DynamoDbContextError::MissingValue)?;
        match value {
            AttributeValue::B(blob) => Ok(blob.as_ref()),
            value => Err(DynamoDbContextError::wrong_value_type(value)),
        }
    }

    /// Extract the value attribute from an item (returned by value).
    fn extract_value_owned(
        attributes: &mut HashMap<String, AttributeValue>,
    ) -> Result<Vec<u8>, DynamoDbContextError> {
        let value = attributes
            .remove(VALUE_ATTRIBUTE)
            .ok_or(DynamoDbContextError::MissingValue)?;
        match value {
            AttributeValue::B(blob) => Ok(blob.into_inner()),
            value => Err(DynamoDbContextError::wrong_value_type(&value)),
        }
    }

    /// Extract the key and value attributes from an item.
    fn extract_key_value(
        prefix_len: usize,
        attributes: &HashMap<String, AttributeValue>,
    ) -> Result<(&[u8], &[u8]), DynamoDbContextError> {
        let key = Self::extract_key(prefix_len, attributes)?;
        let value = Self::extract_value(attributes)?;
        Ok((key, value))
    }

    /// Extract the key and value attributes from an item (returned by value).
    fn extract_key_value_owned(
        prefix_len: usize,
        attributes: &mut HashMap<String, AttributeValue>,
    ) -> Result<(Vec<u8>, Vec<u8>), DynamoDbContextError> {
        let key = Self::extract_key(prefix_len, attributes)?.to_vec();
        let value = Self::extract_value_owned(attributes)?;
        Ok((key, value))
    }

    async fn get_query_output(
        &self,
        attribute_str: &str,
        key_prefix: &[u8],
        start_key_map: Option<HashMap<String, AttributeValue>>,
    ) -> Result<QueryOutput, DynamoDbContextError> {
        let mut response = self
            .client
            .query()
            .table_name(self.table.as_ref())
            .projection_expression(attribute_str)
            .key_condition_expression(format!(
                "{PARTITION_ATTRIBUTE} = :partition and begins_with({KEY_ATTRIBUTE}, :prefix)"
            ))
            .expression_attribute_values(
                ":partition",
                AttributeValue::B(Blob::new(DUMMY_PARTITION_KEY)),
            )
            .expression_attribute_values(":prefix", AttributeValue::B(Blob::new(key_prefix)))
            .set_exclusive_start_key(start_key_map)
            .send()
            .await?;
        Ok(response)
    }
}

// Inspired by https://depth-first.com/articles/2020/06/22/returning-rust-iterators/
#[doc(hidden)]
pub struct DynamoDbKeyIterator {
    key_prefix: Vec<u8>,
    prefix_len: usize,
    DynamoDbClient: client,
    response: Box<QueryOutput>,
    exclusive_start_key: Option<HashMap<String, AttributeValue>>,
    iter: std::iter::Flatten<std::option::Iter<Vec<HashMap<std::string::String, AttributeValue>>>>,
}

/// A set of keys returned by a search query on DynamoDb.
pub struct DynamoDbKeys {
    key_prefix: Vec<u8>,
    DynamoDbClient: client,
}

impl<'a> Iterator for DynamoDbKeyIterator {
    type Item = Result<[u8], DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        match response.last_evaluated_key {
            None => {
                self.iter
                    .next()
                    .map(|x| DynamoDbClient::extract_key(self.prefix_len, x))
            },
            Some(map) => {
                let result = self.iter
                    .next()
                    .map(|x| DynamoDbClient::extract_key(self.prefix_len, x));
                match result {
                    None => {
                        self.response = Box::new(self.get_query_output(KEY_ATTRIBUTE, key_prefix, Some(map)).await?);
                        self.iter = self.response.items.iter.flatten();
                        self.iter
                            .next()
                            .map(|x| DynamoDbClient::extract_key(self.prefix_len, x))
                    },
                    Some(value) => Some(value),
                }
            },
        }
    }
}

impl KeyIterable<DynamoDbContextError> for DynamoDbKeys {
    type Iterator = DynamoDbKeyIterator where Self;

    fn iterator(&self) -> Self::Iterator<'_> {
        let response = Box::new(self.get_query_output(KEY_ATTRIBUTE, key_prefix, None).await?);
        DynamoDbKeyIterator {
            key_prefix: self.key_prefix.clone(),
            prefix_len: self.key_prefix.len(),
            client: self.client.clone(),
            response,
            exclusive_start_key: None,
            iter: self.response.items.iter().flatten(),
        }
    }
}











// Inspired by https://depth-first.com/articles/2020/06/22/returning-rust-iterators/
#[doc(hidden)]
pub struct DynamoDbKeyValueIterator<'a> {
    prefix_len: usize,
    iter: std::iter::Flatten<
        std::option::Iter<'a, Vec<HashMap<std::string::String, AttributeValue>>>,
    >,
}

impl<'a> Iterator for DynamoDbKeyValueIterator<'a> {
    type Item = Result<(&'a [u8], &'a [u8]), DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|x| DynamoDbClient::extract_key_value(self.prefix_len, x))
    }
}

#[doc(hidden)]
pub struct DynamoDbKeyValueIteratorOwned {
    prefix_len: usize,
    iter: std::iter::Flatten<
        std::option::IntoIter<Vec<HashMap<std::string::String, AttributeValue>>>,
    >,
}

impl Iterator for DynamoDbKeyValueIteratorOwned {
    type Item = Result<(Vec<u8>, Vec<u8>), DynamoDbContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|mut x| DynamoDbClient::extract_key_value_owned(self.prefix_len, &mut x))
    }
}

/// A set of key-values returned by a search query on DynamoDb.
pub struct DynamoDbKeyValues {
    prefix_len: usize,
    response: Box<QueryOutput>,
}

impl KeyValueIterable<DynamoDbContextError> for DynamoDbKeyValues {
    type Iterator<'a> = DynamoDbKeyValueIterator<'a> where Self: 'a;
    type IteratorOwned = DynamoDbKeyValueIteratorOwned;

    fn iterator(&self) -> Self::Iterator<'_> {
        DynamoDbKeyValueIterator {
            prefix_len: self.prefix_len,
            iter: self.response.items.iter().flatten(),
        }
    }

    fn into_iterator_owned(self) -> Self::IteratorOwned {
        DynamoDbKeyValueIteratorOwned {
            prefix_len: self.prefix_len,
            iter: self.response.items.into_iter().flatten(),
        }
    }
}








#[async_trait]
impl KeyValueStoreClient for DynamoDbClient {
    type Error = DynamoDbContextError;
    type Keys = DynamoDbKeys;
    type KeyValues = DynamoDbKeyValues;

    async fn read_key_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DynamoDbContextError> {
        let response = self
            .client
            .get_item()
            .table_name(self.table.as_ref())
            .set_key(Some(Self::build_key(key.to_vec())))
            .send()
            .await?;

        match response.item {
            Some(mut item) => Ok(Some(Self::extract_value_owned(&mut item)?)),
            None => Ok(None),
        }
    }

    async fn find_keys_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Self::Keys, DynamoDbContextError> {
        let response = Box::new(self.get_query_output(KEY_ATTRIBUTE, key_prefix, None).await?);
        Ok(DynamoDbKeys {
            prefix_len: key_prefix.len(),
            response,
        })
    }

    async fn find_key_values_by_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Self::KeyValues, DynamoDbContextError> {
        let response : String = Box::new(
            self.get_query_output(KEY_VALUE_ATTRIBUTE, key_prefix, None)
                .await?,
        );
        Ok(DynamoDbKeyValues {
            prefix_len: key_prefix.len(),
            response,
        })
    }

    /// We put submit the transaction in blocks (called BatchWriteItem in dynamoDb) of at most 25
    /// so as to decrease the number of needed transactions. That constant 25 comes from
    /// <https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_BatchWriteItem.html>
    async fn write_batch(&self, batch: Batch) -> Result<(), DynamoDbContextError> {
        let max_size_batch_write_item = 25;
        // We put the delete in insert in separate lists since the use of `DeletePrefix` forces us
        // to download the list of prefix and insert them. Having two lists is preferable as
        // having two types forces us to introduce a new data type that encompass just the Put and Delete.
        let mut delete_list = Vec::new();
        let mut insert_list = Vec::new();
        for op in batch.simplify().operations {
            match op {
                WriteOperation::Delete { key } => {
                    delete_list.push(key);
                }
                WriteOperation::Put { key, value } => {
                    insert_list.push((key, value));
                }
                WriteOperation::DeletePrefix { key_prefix } => {
                    for short_key in self.find_keys_by_prefix(&key_prefix).await?.iterator() {
                        let short_key = short_key?;
                        let mut key = key_prefix.clone();
                        key.extend_from_slice(short_key);
                        delete_list.push(key);
                    }
                }
            };
        }
        for batch_chunk in delete_list.chunks(max_size_batch_write_item) {
            let requests = batch_chunk
                .iter()
                .map(|key| {
                    let request = DeleteRequest::builder()
                        .set_key(Some(Self::build_key(key.to_vec())))
                        .build();
                    WriteRequest::builder().delete_request(request).build()
                })
                .collect();
            self.client
                .batch_write_item()
                .set_request_items(Some(HashMap::from([(self.table.0.clone(), requests)])))
                .send()
                .await?;
        }
        for batch_chunk in insert_list.chunks(max_size_batch_write_item) {
            let requests = batch_chunk
                .iter()
                .map(|(key, value)| {
                    let request = PutRequest::builder()
                        .set_item(Some(Self::build_key_value(key.to_vec(), value.to_vec())))
                        .build();
                    WriteRequest::builder().put_request(request).build()
                })
                .collect();
            self.client
                .batch_write_item()
                .set_request_items(Some(HashMap::from([(self.table.0.clone(), requests)])))
                .send()
                .await?;
        }
        Ok(())
    }
}

impl DynamoDbClient {
    /// Create a new [`DynamoDbClient`] instance.
    pub async fn new(table: TableName) -> Result<(Self, TableStatus), CreateTableError> {
        let config = aws_config::load_from_env().await;

        DynamoDbClient::from_config(&config, table).await
    }
    /// Create the storage table if it doesn't exist.
    ///
    /// Attempts to create the table and ignores errors that indicate that it already exists.
    async fn create_table_if_needed(&self) -> Result<TableStatus, CreateTableError> {
        let result = self
            .client
            .create_table()
            .table_name(self.table.as_ref())
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(PARTITION_ATTRIBUTE)
                    .attribute_type(ScalarAttributeType::B)
                    .build(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(KEY_ATTRIBUTE)
                    .attribute_type(ScalarAttributeType::B)
                    .build(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(PARTITION_ATTRIBUTE)
                    .key_type(KeyType::Hash)
                    .build(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(KEY_ATTRIBUTE)
                    .key_type(KeyType::Range)
                    .build(),
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .read_capacity_units(10)
                    .write_capacity_units(10)
                    .build(),
            )
            .send()
            .await;

        match result {
            Ok(_) => Ok(TableStatus::New),
            Err(error) if error.is_resource_in_use_exception() => Ok(TableStatus::Existing),
            Err(error) => Err(error.into()),
        }
    }

    /// Create a new [`DynamoDbClient`] instance using the provided `config` parameters.
    pub async fn from_config(
        config: impl Into<Config>,
        table: TableName,
    ) -> Result<(Self, TableStatus), CreateTableError> {
        let db = DynamoDbClient {
            client: Client::from_conf(config.into()),
            table,
        };

        let table_status = db.create_table_if_needed().await?;

        Ok((db, table_status))
    }

    /// Create a new [`DynamoDbClient`] instance using a LocalStack endpoint.
    ///
    /// Requires a `LOCALSTACK_ENDPOINT` environment variable with the endpoint address to connect
    /// to the LocalStack instance. Creates the table if it doesn't exist yet, reporting a
    /// [`TableStatus`] to indicate if the table was created or if it already exists.
    pub async fn with_localstack(table: TableName) -> Result<(Self, TableStatus), LocalStackError> {
        let base_config = aws_config::load_from_env().await;
        let config = aws_sdk_dynamodb::config::Builder::from(&base_config)
            .endpoint_resolver(localstack::get_endpoint()?)
            .build();

        Ok(DynamoDbClient::from_config(config, table).await?)
    }
}

impl<E> DynamoDbContext<E>
where
    E: Clone + Sync + Send,
{
    fn create_context(
        db_tablestatus: (DynamoDbClient, TableStatus),
        base_key: Vec<u8>,
        extra: E,
    ) -> (Self, TableStatus) {
        let storage = DynamoDbContext {
            db: db_tablestatus.0,
            base_key,
            extra,
        };
        (storage, db_tablestatus.1)
    }

    /// Create a new [`DynamoDbContext`] instance.
    pub async fn new(
        table: TableName,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), CreateTableError> {
        let db_tablestatus = DynamoDbClient::new(table).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }

    /// Create a new [`DynamoDbContext`] instance from the given AWS configuration.
    pub async fn from_config(
        config: impl Into<Config>,
        table: TableName,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), CreateTableError> {
        let db_tablestatus = DynamoDbClient::from_config(config, table).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }

    /// Create a new [`DynamoDbContext`] instance using a LocalStack endpoint.
    ///
    /// Requires a `LOCALSTACK_ENDPOINT` environment variable with the endpoint address to connect
    /// to the LocalStack instance. Creates the table if it doesn't exist yet, reporting a
    /// [`TableStatus`] to indicate if the table was created or if it already exists.
    pub async fn with_localstack(
        table: TableName,
        base_key: Vec<u8>,
        extra: E,
    ) -> Result<(Self, TableStatus), LocalStackError> {
        let db_tablestatus = DynamoDbClient::with_localstack(table).await?;
        Ok(Self::create_context(db_tablestatus, base_key, extra))
    }

    /// Clone this [`DynamoDbContext`] while entering a sub-scope.
    ///
    /// The return context has its key prefix extended with `scope_prefix` and uses the
    /// `new_extra` instead of cloning the current extra data.
    pub fn clone_with_sub_scope<NewE: Clone + Send + Sync>(
        &self,
        scope_prefix: &impl Serialize,
        new_extra: NewE,
    ) -> Result<DynamoDbContext<NewE>, DynamoDbContextError> {
        Ok(DynamoDbContext {
            db: self.db.clone(),
            base_key: self.derive_key(scope_prefix)?,
            extra: new_extra,
        })
    }
}

/// Status of a table at the creation time of a [`DynamoDbContext`] instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableStatus {
    /// Table was created during the construction of the [`DynamoDbContext`] instance.
    New,
    /// Table already existed when the [`DynamoDbContext`] instance was created.
    Existing,
}

/// A DynamoDB table name.
///
/// Table names must follow some [naming
/// rules](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/HowItWorks.NamingRulesDataTypes.html#HowItWorks.NamingRules),
/// so this type ensures that they are properly validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableName(String);

impl FromStr for TableName {
    type Err = InvalidTableName;

    fn from_str(string: &str) -> Result<Self, Self::Err> {
        if string.len() < 3 {
            return Err(InvalidTableName::TooShort);
        }
        if string.len() > 255 {
            return Err(InvalidTableName::TooLong);
        }
        if !string.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '.'
                || character == '-'
                || character == '_'
        }) {
            return Err(InvalidTableName::InvalidCharacter);
        }
        Ok(TableName(string.to_owned()))
    }
}

impl AsRef<String> for TableName {
    fn as_ref(&self) -> &String {
        &self.0
    }
}

/// Error when validating a table name.
#[derive(Debug, Error)]
pub enum InvalidTableName {
    /// The table name should be at least 3 characters
    #[error("Table name must have at least 3 characters")]
    TooShort,

    /// The table name should be at most 63 characters
    #[error("Table name must be at most 63 characters")]
    TooLong,

    /// allowed characters are lowercase letters, numbers, periods and hyphens
    #[error("Table name must only contain lowercase letters, numbers, periods and hyphens")]
    InvalidCharacter,
}

/// Errors that occur when using [`DynamoDbContext`].
#[derive(Debug, Error)]
pub enum DynamoDbContextError {
    /// An error occurred while putting the item
    #[error(transparent)]
    Put(#[from] Box<SdkError<aws_sdk_dynamodb::error::PutItemError>>),

    /// An error occurred while getting the item
    #[error(transparent)]
    Get(#[from] Box<SdkError<aws_sdk_dynamodb::error::GetItemError>>),

    /// An error occurred while deleting the item
    #[error(transparent)]
    Delete(#[from] Box<SdkError<aws_sdk_dynamodb::error::DeleteItemError>>),

    /// An error occurred while writing a batch of item
    #[error(transparent)]
    BatchWriteItem(#[from] Box<SdkError<aws_sdk_dynamodb::error::BatchWriteItemError>>),

    /// An error occurred while doing a Query
    #[error(transparent)]
    Query(#[from] Box<SdkError<aws_sdk_dynamodb::error::QueryError>>),

    /// The stored key is missing
    #[error("The stored key attribute is missing")]
    MissingKey,

    /// The type of the keys was not correct (It should have been a binary blob)
    #[error("Key was stored as {0}, but it was expected to be stored as a binary blob")]
    WrongKeyType(String),

    /// The value attribute is missing
    #[error("The stored value attribute is missing")]
    MissingValue,

    /// The value was stored as the wrong type (it should be a binary blob)
    #[error("Value was stored as {0}, but it was expected to be stored as a binary blob")]
    WrongValueType(String),

    /// A BCS error occurred
    #[error(transparent)]
    BcsError(#[from] bcs::Error),

    /// An error occurred while creating the table
    #[error(transparent)]
    CreateTable(#[from] Box<CreateTableError>),

    /// The item was not found
    #[error("Item not found in DynamoDB table: {0}")]
    NotFound(String),
}

impl<InnerError> From<SdkError<InnerError>> for DynamoDbContextError
where
    DynamoDbContextError: From<Box<SdkError<InnerError>>>,
{
    fn from(error: SdkError<InnerError>) -> Self {
        Box::new(error).into()
    }
}

impl From<CreateTableError> for DynamoDbContextError {
    fn from(error: CreateTableError) -> Self {
        Box::new(error).into()
    }
}

impl DynamoDbContextError {
    /// Create a [`DynamoDbContextError::WrongKeyType`] instance based on the returned value type.
    ///
    /// # Panics
    ///
    /// If the value type is in the correct type, a binary blob.
    pub fn wrong_key_type(value: &AttributeValue) -> Self {
        DynamoDbContextError::WrongKeyType(Self::type_description_of(value))
    }

    /// Create a [`DynamoDbContextError::WrongValueType`] instance based on the returned value type.
    ///
    /// # Panics
    ///
    /// If the value type is in the correct type, a binary blob.
    pub fn wrong_value_type(value: &AttributeValue) -> Self {
        DynamoDbContextError::WrongValueType(Self::type_description_of(value))
    }

    fn type_description_of(value: &AttributeValue) -> String {
        match value {
            AttributeValue::B(_) => unreachable!("creating an error type for the correct type"),
            AttributeValue::Bool(_) => "a boolean",
            AttributeValue::Bs(_) => "a list of binary blobs",
            AttributeValue::L(_) => "a list",
            AttributeValue::M(_) => "a map",
            AttributeValue::N(_) => "a number",
            AttributeValue::Ns(_) => "a list of numbers",
            AttributeValue::Null(_) => "a null value",
            AttributeValue::S(_) => "a string",
            AttributeValue::Ss(_) => "a list of strings",
            _ => "an unknown type",
        }
        .to_owned()
    }
}

impl From<DynamoDbContextError> for crate::views::ViewError {
    fn from(error: DynamoDbContextError) -> Self {
        Self::ContextError {
            backend: "DynamoDB".to_string(),
            error: error.to_string(),
        }
    }
}

/// Error when creating a table for a new [`DynamoDbContext`] instance.
#[derive(Debug, Error)]
pub enum CreateTableError {
    /// An error occurred while creating the table
    #[error(transparent)]
    CreateTable(#[from] SdkError<aws_sdk_dynamodb::error::CreateTableError>),
}

/// Error when creating a [`DynamoDbContext`] instance using a LocalStack instance.
#[derive(Debug, Error)]
pub enum LocalStackError {
    /// An Endpoint error occurred
    #[error(transparent)]
    Endpoint(#[from] localstack::EndpointError),

    /// An error occurred while creating the table
    #[error(transparent)]
    CreateTable(#[from] Box<CreateTableError>),
}

impl From<CreateTableError> for LocalStackError {
    fn from(error: CreateTableError) -> Self {
        Box::new(error).into()
    }
}

/// A helper trait to add a `SdkError<CreateTableError>::is_resource_in_use_exception()` method.
trait IsResourceInUseException {
    /// Check if the error is a resource is in use exception.
    fn is_resource_in_use_exception(&self) -> bool;
}

impl IsResourceInUseException for SdkError<aws_sdk_dynamodb::error::CreateTableError> {
    fn is_resource_in_use_exception(&self) -> bool {
        matches!(
            self,
            SdkError::ServiceError {
                err: aws_sdk_dynamodb::error::CreateTableError {
                    kind: aws_sdk_dynamodb::error::CreateTableErrorKind::ResourceInUseException(_),
                    ..
                },
                ..
            }
        )
    }
}
