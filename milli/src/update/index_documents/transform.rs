use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use fst::MapBuilder;
use heed::types::OwnedType;
use heed::Database;
use itertools::Itertools;
use log::info;
use obkv::KvWriter;
use roaring::RoaringBitmap;
use serde_json::{Map, Value};

use super::helpers::{
    create_sorter, create_writer, keep_latest_obkv, merge_obkvs, merge_two_obkvs, MergeFn,
};
use super::{IndexDocumentsMethod, IndexerConfig};
use crate::documents::{DocumentBatchReader, DocumentsBatchIndex};
use crate::error::{Error, InternalError, UserError};
use crate::index::db_name;
use crate::update::{AvailableDocumentsIds, UpdateIndexingStep};
use crate::{
    ExternalDocumentsIds, FieldDistribution, FieldId, FieldsIdsMap, Index, ObkvCodec, Result, BEU32,
};

const DEFAULT_PRIMARY_KEY_NAME: &str = "id";

pub struct TransformOutput {
    pub primary_key: String,
    pub fields_ids_map: FieldsIdsMap,
    pub field_distribution: FieldDistribution,
    pub external_documents_ids: ExternalDocumentsIds<'static>,
    pub new_documents_ids: RoaringBitmap,
    pub replaced_documents_ids: RoaringBitmap,
    pub documents_count: usize,
    pub original_documents: File,
    pub flattened_documents: File,
}

/// Extract the external ids, deduplicate and compute the new internal documents ids
/// and fields ids, writing all the documents under their internal ids into a final file.
///
/// Outputs the new `FieldsIdsMap`, the new `UsersIdsDocumentsIds` map, the new documents ids,
/// the replaced documents ids, the number of documents in this update and the file
/// containing all those documents.
pub struct Transform<'a, 'i> {
    pub index: &'i Index,
    indexer_settings: &'a IndexerConfig,
    pub autogenerate_docids: bool,
    pub index_documents_method: IndexDocumentsMethod,

    original_sorter: grenad::Sorter<MergeFn>,
    documents_count: usize,
}

/// Create a mapping between the field ids found in the document batch and the one that were
/// already present in the index.
///
/// If new fields are present in the addition, they are added to the index field ids map.
fn create_fields_mapping(
    index_field_map: &mut FieldsIdsMap,
    batch_field_map: &DocumentsBatchIndex,
) -> Result<HashMap<FieldId, FieldId>> {
    batch_field_map
        .iter()
        // we sort by id here to ensure a deterministic mapping of the fields, that preserves
        // the original ordering.
        .sorted_by_key(|(&id, _)| id)
        .map(|(field, name)| match index_field_map.id(&name) {
            Some(id) => Ok((*field, id)),
            None => index_field_map
                .insert(&name)
                .ok_or(Error::UserError(UserError::AttributeLimitReached))
                .map(|id| (*field, id)),
        })
        .collect()
}

fn find_primary_key(index: &DocumentsBatchIndex) -> Option<&str> {
    index
        .iter()
        .sorted_by_key(|(k, _)| *k)
        .map(|(_, v)| v)
        .find(|v| v.to_lowercase().contains(DEFAULT_PRIMARY_KEY_NAME))
        .map(String::as_str)
}

impl<'a, 'i> Transform<'a, 'i> {
    pub fn new(
        index: &'i Index,
        indexer_settings: &'a IndexerConfig,
        index_documents_method: IndexDocumentsMethod,
        autogenerate_docids: bool,
    ) -> Self {
        // We must choose the appropriate merge function for when two or more documents
        // with the same user id must be merged or fully replaced in the same batch.
        let merge_function = match index_documents_method {
            IndexDocumentsMethod::ReplaceDocuments => keep_latest_obkv,
            IndexDocumentsMethod::UpdateDocuments => merge_obkvs,
        };

        // We initialize the sorter with the user indexing settings.
        let original_sorter = create_sorter(
            merge_function,
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            indexer_settings.max_nb_chunks,
            indexer_settings.max_memory,
        );

        Transform {
            index,
            indexer_settings,
            autogenerate_docids,
            original_sorter,
            documents_count: 0,
            index_documents_method,
        }
    }

    pub fn read_documents<R, F>(
        &mut self,
        mut reader: DocumentBatchReader<R>,
        wtxn: &mut heed::RwTxn,
        progress_callback: F,
    ) -> Result<usize>
    where
        R: Read + Seek,
        F: Fn(UpdateIndexingStep) + Sync,
    {
        let fields_index = reader.index();
        let mut fields_ids_map = self.index.fields_ids_map(wtxn)?;
        let mapping = create_fields_mapping(&mut fields_ids_map, fields_index)?;

        let alternative_name = self
            .index
            .primary_key(wtxn)?
            .or_else(|| find_primary_key(fields_index))
            .map(String::from);

        let (primary_key_id, primary_key_name) = compute_primary_key_pair(
            self.index.primary_key(wtxn)?,
            &mut fields_ids_map,
            alternative_name,
            self.autogenerate_docids,
        )?;

        let mut obkv_buffer = Vec::new();
        let mut documents_count = 0;
        let mut external_id_buffer = Vec::new();
        let mut field_buffer: Vec<(u16, &[u8])> = Vec::new();
        while let Some((addition_index, document)) = reader.next_document_with_index()? {
            let mut field_buffer_cache = drop_and_reuse(field_buffer);
            if self.indexer_settings.log_every_n.map_or(false, |len| documents_count % len == 0) {
                progress_callback(UpdateIndexingStep::RemapDocumentAddition {
                    documents_seen: documents_count,
                });
            }

            for (k, v) in document.iter() {
                let mapped_id = *mapping.get(&k).unwrap();
                field_buffer_cache.push((mapped_id, v));
            }

            // We need to make sure that every document has a primary key. After we have remapped
            // all the fields in the document, we try to find the primary key value. If we can find
            // it, transform it into a string and validate it, and then update it in the
            // document. If none is found, and we were told to generate missing document ids, then
            // we create the missing field, and update the new document.
            let mut uuid_buffer = [0; uuid::adapter::Hyphenated::LENGTH];
            let external_id =
                match field_buffer_cache.iter_mut().find(|(id, _)| *id == primary_key_id) {
                    Some((_, bytes)) => {
                        let value = match serde_json::from_slice(bytes).unwrap() {
                            Value::String(string) => match validate_document_id(&string) {
                                Some(s) if s.len() == string.len() => string,
                                Some(s) => s.to_string(),
                                None => {
                                    return Err(UserError::InvalidDocumentId {
                                        document_id: Value::String(string),
                                    }
                                    .into())
                                }
                            },
                            Value::Number(number) => number.to_string(),
                            content => {
                                return Err(UserError::InvalidDocumentId {
                                    document_id: content.clone(),
                                }
                                .into())
                            }
                        };
                        serde_json::to_writer(&mut external_id_buffer, &value).unwrap();
                        Cow::Owned(value)
                    }
                    None => {
                        if !self.autogenerate_docids {
                            let mut json = Map::new();
                            for (key, value) in document.iter() {
                                let key = addition_index.name(key).cloned();
                                let value = serde_json::from_slice::<Value>(&value).ok();

                                if let Some((k, v)) = key.zip(value) {
                                    json.insert(k, v);
                                }
                            }

                            return Err(UserError::MissingDocumentId {
                                primary_key: primary_key_name,
                                document: json,
                            }
                            .into());
                        }

                        let uuid =
                            uuid::Uuid::new_v4().to_hyphenated().encode_lower(&mut uuid_buffer);
                        serde_json::to_writer(&mut external_id_buffer, &uuid).unwrap();
                        field_buffer_cache.push((primary_key_id, &external_id_buffer));
                        Cow::Borrowed(&*uuid)
                    }
                };

            // Insertion in a obkv need to be done with keys ordered. For now they are ordered
            // according to the document addition key order, so we sort it according to the
            // fieldids map keys order.
            field_buffer_cache.sort_unstable_by(|(f1, _), (f2, _)| f1.cmp(&f2));

            // The last step is to build the new obkv document, and insert it in the sorter.
            let mut writer = obkv::KvWriter::new(&mut obkv_buffer);
            for (k, v) in field_buffer_cache.iter() {
                writer.insert(*k, v)?;
            }

            // We use the extracted/generated user id as the key for this document.
            self.original_sorter.insert(&external_id.as_ref().as_bytes(), &obkv_buffer)?;
            documents_count += 1;

            progress_callback(UpdateIndexingStep::RemapDocumentAddition {
                documents_seen: documents_count,
            });

            obkv_buffer.clear();
            field_buffer = drop_and_reuse(field_buffer_cache);
            external_id_buffer.clear();
        }

        progress_callback(UpdateIndexingStep::RemapDocumentAddition {
            documents_seen: documents_count,
        });

        self.index.put_fields_ids_map(wtxn, &fields_ids_map)?;
        self.index.put_primary_key(wtxn, &primary_key_name)?;
        self.documents_count += documents_count;
        // Now that we have a valid sorter that contains the user id and the obkv we
        // give it to the last transforming function which returns the TransformOutput.
        Ok(documents_count)
    }

    /// Generate the `TransformOutput` based on the given sorter that can be generated from any
    /// format like CSV, JSON or JSON stream. This sorter must contain a key that is the document
    /// id for the user side and the value must be an obkv where keys are valid fields ids.
    pub(crate) fn output_from_sorter<F>(
        self,
        wtxn: &mut heed::RwTxn,
        progress_callback: F,
    ) -> Result<TransformOutput>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        let primary_key = self
            .index
            .primary_key(&wtxn)?
            .ok_or(Error::UserError(UserError::MissingPrimaryKey))?
            .to_string();
        let mut fields_ids_map = self.index.fields_ids_map(wtxn)?;

        let mut documents_ids = self.index.documents_ids(wtxn)?;
        let mut field_distribution = self.index.field_distribution(wtxn)?;
        let mut external_documents_ids = self.index.external_documents_ids(wtxn)?;

        let total_documents = self.documents_count;

        let mut new_documents_ids = RoaringBitmap::new();

        let (documents_count, replaced_documents_ids, original_documents, flattened_documents) =
            Self::write_final_sorter(
                wtxn,
                self.original_sorter,
                self.indexer_settings,
                &self.index.documents,
                self.index_documents_method,
                total_documents,
                &mut field_distribution,
                &mut fields_ids_map,
                &mut documents_ids,
                &mut external_documents_ids,
                &mut new_documents_ids,
                &progress_callback,
            )?;

        progress_callback(UpdateIndexingStep::ComputeIdsAndMergeDocuments {
            documents_seen: documents_count,
            total_documents: documents_count,
        });

        Ok(TransformOutput {
            primary_key,
            fields_ids_map,
            field_distribution,
            external_documents_ids: external_documents_ids.into_static(),
            new_documents_ids,
            replaced_documents_ids,
            documents_count,
            original_documents,
            flattened_documents,
        })
    }

    /// TODO: TAMO: documentation
    pub(crate) fn write_final_sorter<F>(
        wtxn: &heed::RwTxn,
        // normal parameters
        original_sorter: grenad::Sorter<MergeFn>,
        // what should be in self
        indexer_settings: &IndexerConfig,
        documents: &Database<OwnedType<BEU32>, ObkvCodec>,
        index_documents_method: IndexDocumentsMethod,
        total_documents: usize,
        field_distribution: &mut FieldDistribution,
        fields_ids_map: &mut FieldsIdsMap,
        documents_ids: &mut RoaringBitmap,
        external_documents_ids: &mut ExternalDocumentsIds,
        new_documents_ids: &mut RoaringBitmap,
        progress_callback: F,
    ) -> Result<(usize, RoaringBitmap, File, File)>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        let mut obkv_buffer = Vec::new();
        let mut replaced_documents_ids = RoaringBitmap::new();
        let mut new_external_documents_ids_builder = fst::MapBuilder::memory();
        let mut available_documents_ids = AvailableDocumentsIds::from_documents_ids(documents_ids);

        // Once we have sort and deduplicated the documents we write them into a final file.
        let mut original_final_sorter = create_sorter(
            |_id, obkvs| {
                if obkvs.len() == 1 {
                    Ok(obkvs[0].clone())
                } else {
                    Err(InternalError::IndexingMergingKeys { process: "documents" }.into())
                }
            },
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            indexer_settings.max_nb_chunks,
            indexer_settings.max_memory,
        );

        // Once we have sort and deduplicated the documents we write their flattened version into a final file.
        let mut flattened_final_sorter = create_sorter(
            |_id, obkvs| {
                if obkvs.len() == 1 {
                    Ok(obkvs[0].clone())
                } else {
                    Err(InternalError::IndexingMergingKeys { process: "documents" }.into())
                }
            },
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            indexer_settings.max_nb_chunks,
            indexer_settings.max_memory,
        );

        // consume sorter, in order to free the internal allocation, before creating a new one.
        let mut iter = original_sorter.into_stream_merger_iter()?;

        // While we write into final file we get or generate the internal documents ids.
        let mut documents_count = 0;

        while let Some((external_id, update_obkv)) = iter.next()? {
            if indexer_settings.log_every_n.map_or(false, |len| documents_count % len == 0) {
                progress_callback(UpdateIndexingStep::ComputeIdsAndMergeDocuments {
                    documents_seen: documents_count,
                    total_documents,
                });
            }

            let (docid, updated) = match external_documents_ids.get(external_id) {
                Some(docid) => (docid, true),
                None => {
                    let new_docid =
                        available_documents_ids.next().ok_or(UserError::DocumentLimitReached)?;
                    (new_docid, false)
                }
            };

            let obkv = Self::add_and_merge_original_document(
                wtxn,
                updated,
                external_id,
                docid,
                update_obkv,
                &mut obkv_buffer,
                documents,
                index_documents_method,
                field_distribution,
                fields_ids_map,
                &mut replaced_documents_ids,
                new_documents_ids,
                &mut new_external_documents_ids_builder,
            )?;

            // We insert the document under the documents ids map into the final file.
            original_final_sorter.insert(docid.to_be_bytes(), obkv)?;
            documents_count += 1;

            // Once we have the final document. We're going to flatten it
            // and insert it in the flattened sorter.
            let mut doc = serde_json::Map::new();

            let reader = obkv::KvReader::new(obkv);
            for (k, v) in reader.iter() {
                let key = fields_ids_map.name(k).unwrap();
                let value = serde_json::from_slice::<serde_json::Value>(v)
                    .map_err(crate::error::InternalError::SerdeJson)?;
                doc.insert(key.to_string(), value);
            }

            let flattened = flatten_serde_json::flatten(&doc);

            // Once we have the flattened version we can convert it back to obkv and
            // insert all the new generated fields_ids (if any) in the fields ids map.
            let mut buffer: Vec<u8> = Vec::new();
            let mut writer = KvWriter::new(&mut buffer);
            for (key, value) in flattened {
                let fid = fields_ids_map.insert(&key).ok_or(UserError::AttributeLimitReached)?;
                let value = serde_json::to_vec(&value).unwrap();
                writer.insert(fid, &value)?;
            }

            flattened_final_sorter.insert(docid.to_be_bytes(), &buffer)?;
        }

        let before_docids_merging = Instant::now();
        // We merge the new external ids with existing external documents ids.
        let new_external_documents_ids = new_external_documents_ids_builder.into_map();
        external_documents_ids.insert_ids(&new_external_documents_ids)?;
        info!("Documents external merging took {:.02?}", before_docids_merging.elapsed());

        // We create a final writer to write the new documents in order from the sorter.
        let mut writer = create_writer(
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        // Once we have written all the documents into the final sorter, we write the documents
        // into this writer, extract the file and reset the seek to be able to read it again.
        original_final_sorter.write_into_stream_writer(&mut writer)?;
        let mut original_documents = writer.into_inner()?;
        original_documents.seek(SeekFrom::Start(0))?;

        // We create a final writer to write the new documents in order from the sorter.
        let mut writer = create_writer(
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        // Once we have written all the documents into the final sorter, we write the documents
        // into this writer, extract the file and reset the seek to be able to read it again.
        flattened_final_sorter.write_into_stream_writer(&mut writer)?;
        let mut flattened_documents = writer.into_inner()?;
        flattened_documents.seek(SeekFrom::Start(0))?;

        Ok((documents_count, replaced_documents_ids, original_documents, flattened_documents))
    }

    /// Update all the informations concerning a document addition.
    /// - Update the fields_ids_map with the new fields and delete the old fields.
    /// - Update the field_distribution
    /// - Generate the new document according to the merge strategy
    /// Must only be called when updating an original document.
    /// (= do not call it when updating a flattened document)
    pub fn add_and_merge_original_document<'buffer>(
        wtxn: &heed::RwTxn,
        // normal parameters
        updated: bool,
        external_id: &[u8],
        docid: u32,
        document: &'buffer [u8],
        obkv_buffer: &'buffer mut Vec<u8>,
        // what should be in self
        documents: &Database<OwnedType<BEU32>, ObkvCodec>,
        index_documents_method: IndexDocumentsMethod,
        field_distribution: &mut FieldDistribution,
        fields_ids_map: &mut FieldsIdsMap,
        // I don't know for these three
        replaced_documents_ids: &mut RoaringBitmap,
        new_documents_ids: &mut RoaringBitmap,
        new_external_documents_ids_builder: &mut MapBuilder<Vec<u8>>,
    ) -> Result<&'buffer [u8]> {
        let obkv = if updated {
            // If we find the user id in the current external documents ids map
            // we use it and insert it in the list of replaced documents.
            replaced_documents_ids.insert(docid);

            let key = BEU32::new(docid);
            let base_obkv =
                documents.get(wtxn, &key)?.ok_or(InternalError::DatabaseMissingEntry {
                    db_name: db_name::DOCUMENTS,
                    key: None,
                })?;

            // we remove all the fields that were already counted
            for (field_id, _) in base_obkv.iter() {
                let field_name = fields_ids_map.name(field_id).unwrap();
                if let Entry::Occupied(mut entry) = field_distribution.entry(field_name.to_string())
                {
                    match entry.get().checked_sub(1) {
                        Some(0) | None => entry.remove(),
                        Some(count) => entry.insert(count),
                    };
                }
            }

            // Depending on the update indexing method we will merge
            // the document update with the current document or not.
            match index_documents_method {
                IndexDocumentsMethod::ReplaceDocuments => document,
                IndexDocumentsMethod::UpdateDocuments => {
                    let document = obkv::KvReader::new(document);
                    merge_two_obkvs(base_obkv, document, obkv_buffer);
                    obkv_buffer.as_slice()
                }
            }
        } else {
            new_external_documents_ids_builder.insert(external_id, docid as u64)?;
            new_documents_ids.insert(docid);
            document
        };

        let reader = obkv::KvReader::new(obkv);
        for (field_id, _) in reader.iter() {
            let field_name = fields_ids_map.name(field_id).unwrap();
            *field_distribution.entry(field_name.to_string()).or_default() += 1;
        }

        Ok(obkv)
    }

    /// Returns a `TransformOutput` with a file that contains the documents of the index
    /// with the attributes reordered accordingly to the `FieldsIdsMap` given as argument.
    // TODO this can be done in parallel by using the rayon `ThreadPool`.
    pub fn remap_index_documents(
        self,
        wtxn: &mut heed::RwTxn,
        old_fields_ids_map: FieldsIdsMap,
        new_fields_ids_map: FieldsIdsMap,
    ) -> Result<TransformOutput> {
        // There already has been a document addition, the primary key should be set by now.
        let primary_key =
            self.index.primary_key(wtxn)?.ok_or(UserError::MissingPrimaryKey)?.to_string();
        let field_distribution = self.index.field_distribution(wtxn)?;
        let external_documents_ids = self.index.external_documents_ids(wtxn)?;
        let documents_ids = self.index.documents_ids(wtxn)?;
        let documents_count = documents_ids.len() as usize;

        // We create a final writer to write the new documents in order from the sorter.
        let mut original_writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        let mut obkv_buffer = Vec::new();
        for result in self.index.documents.iter(wtxn)? {
            let (docid, obkv) = result?;
            let docid = docid.get();

            obkv_buffer.clear();
            let mut obkv_writer = obkv::KvWriter::<_, FieldId>::new(&mut obkv_buffer);

            // We iterate over the new `FieldsIdsMap` ids in order and construct the new obkv.
            for (id, name) in new_fields_ids_map.iter() {
                if let Some(val) = old_fields_ids_map.id(name).and_then(|id| obkv.get(id)) {
                    obkv_writer.insert(id, val)?;
                }
            }

            let buffer = obkv_writer.into_inner()?;
            original_writer.insert(docid.to_be_bytes(), buffer)?;
        }

        // Once we have written all the documents, we extract
        // the file and reset the seek to be able to read it again.
        let mut original_documents = original_writer.into_inner()?;
        original_documents.seek(SeekFrom::Start(0))?;

        // TODO: TAMO
        // We create a final writer to write the new documents in order from the sorter.
        let flattened_writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );
        let mut flattened_documents = flattened_writer.into_inner()?;
        flattened_documents.seek(SeekFrom::Start(0))?;

        Ok(TransformOutput {
            primary_key,
            fields_ids_map: new_fields_ids_map,
            field_distribution,
            external_documents_ids: external_documents_ids.into_static(),
            new_documents_ids: documents_ids,
            replaced_documents_ids: RoaringBitmap::default(),
            documents_count,
            original_documents,
            flattened_documents,
        })
    }
}

/// Given an optional primary key and an optional alternative name, returns the (field_id, attr_name)
/// for the primary key according to the following rules:
/// - if primary_key is `Some`, returns the id and the name, else
/// - if alternative_name is Some, adds alternative to the fields_ids_map, and returns the pair, else
/// - if autogenerate_docids is true, insert the default id value in the field ids map ("id") and
/// returns the pair, else
/// - returns an error.
fn compute_primary_key_pair(
    primary_key: Option<&str>,
    fields_ids_map: &mut FieldsIdsMap,
    alternative_name: Option<String>,
    autogenerate_docids: bool,
) -> Result<(FieldId, String)> {
    match primary_key {
        Some(primary_key) => {
            let id = fields_ids_map.insert(primary_key).ok_or(UserError::AttributeLimitReached)?;
            Ok((id, primary_key.to_string()))
        }
        None => {
            let name = match alternative_name {
                Some(key) => key,
                None => {
                    if !autogenerate_docids {
                        // If there is no primary key in the current document batch, we must
                        // return an error and not automatically generate any document id.
                        return Err(UserError::MissingPrimaryKey.into());
                    }
                    DEFAULT_PRIMARY_KEY_NAME.to_string()
                }
            };
            let id = fields_ids_map.insert(&name).ok_or(UserError::AttributeLimitReached)?;
            Ok((id, name))
        }
    }
}

fn validate_document_id(document_id: &str) -> Option<&str> {
    let document_id = document_id.trim();
    Some(document_id).filter(|id| {
        !id.is_empty()
            && id.chars().all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'))
    })
}

/// Drops all the value of type `U` in vec, and reuses the allocation to create a `Vec<T>`.
///
/// The size and alignment of T and U must match.
fn drop_and_reuse<U, T>(mut vec: Vec<U>) -> Vec<T> {
    debug_assert_eq!(std::mem::align_of::<U>(), std::mem::align_of::<T>());
    debug_assert_eq!(std::mem::size_of::<U>(), std::mem::size_of::<T>());
    vec.clear();
    debug_assert!(vec.is_empty());
    vec.into_iter().map(|_| unreachable!()).collect()
}

#[cfg(test)]
mod test {
    use super::*;

    mod compute_primary_key {
        use super::{compute_primary_key_pair, FieldsIdsMap};

        #[test]
        fn should_return_primary_key_if_is_some() {
            let mut fields_map = FieldsIdsMap::new();
            fields_map.insert("toto").unwrap();
            let result = compute_primary_key_pair(
                Some("toto"),
                &mut fields_map,
                Some("tata".to_string()),
                false,
            );
            assert_eq!(result.unwrap(), (0, "toto".to_string()));
            assert_eq!(fields_map.len(), 1);
        }

        #[test]
        fn should_return_alternative_if_primary_is_none() {
            let mut fields_map = FieldsIdsMap::new();
            let result =
                compute_primary_key_pair(None, &mut fields_map, Some("tata".to_string()), false);
            assert_eq!(result.unwrap(), (0, "tata".to_string()));
            assert_eq!(fields_map.len(), 1);
        }

        #[test]
        fn should_return_default_if_both_are_none() {
            let mut fields_map = FieldsIdsMap::new();
            let result = compute_primary_key_pair(None, &mut fields_map, None, true);
            assert_eq!(result.unwrap(), (0, "id".to_string()));
            assert_eq!(fields_map.len(), 1);
        }

        #[test]
        fn should_return_err_if_both_are_none_and_recompute_is_false() {
            let mut fields_map = FieldsIdsMap::new();
            let result = compute_primary_key_pair(None, &mut fields_map, None, false);
            assert!(result.is_err());
            assert_eq!(fields_map.len(), 0);
        }
    }

    mod primary_key_inference {
        use bimap::BiHashMap;

        use crate::documents::DocumentsBatchIndex;
        use crate::update::index_documents::transform::find_primary_key;

        #[test]
        fn primary_key_infered_on_first_field() {
            // We run the test multiple times to change the order in which the fields are iterated upon.
            for _ in 1..50 {
                let mut map = BiHashMap::new();
                map.insert(1, "fakeId".to_string());
                map.insert(2, "fakeId".to_string());
                map.insert(3, "fakeId".to_string());
                map.insert(4, "fakeId".to_string());
                map.insert(0, "realId".to_string());

                assert_eq!(find_primary_key(&DocumentsBatchIndex(map)), Some("realId"));
            }
        }
    }
}
