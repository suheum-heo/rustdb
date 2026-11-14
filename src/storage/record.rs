//! Variable-length record codec.
//!
//! Integers are little-endian two's complement. Text is a byte length followed
//! by UTF-8. Booleans are the bytes `0` and `1`. `NULL` is a tag with no
//! payload. None of these are Rust's in-memory representation.

use crate::error::DbError;
use crate::schema::Column;
use crate::storage::page::{MAX_RECORD_LEN, corrupt_file};
use crate::value::{DataType, Value};

const TAG_NULL: u8 = 0;
const TAG_INTEGER: u8 = 1;
const TAG_TEXT: u8 = 2;
const TAG_BOOLEAN: u8 = 3;

const TYPE_INTEGER: u8 = 1;
const TYPE_TEXT: u8 = 2;
const TYPE_BOOLEAN: u8 = 3;

/// Fixed prefix of a heap record: id, links, and the value count.
const RECORD_PREFIX: usize = 22;

/// A null row pointer. Page 0 is the meta page, so it is never a heap slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Link {
    pub page: u32,
    pub slot: u16,
}

pub(crate) const NULL_LINK: Link = Link { page: 0, slot: 0 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    pub row_id: u64,
    pub prev: Link,
    pub next: Link,
    pub values: Vec<Value>,
}

pub(crate) fn ensure_record_fits(values: &[Value]) -> Result<(), DbError> {
    let length = encoded_record_len(values)?;
    if length > MAX_RECORD_LEN {
        Err(DbError::Storage(format!(
            "row exceeds the maximum record size of {MAX_RECORD_LEN} bytes"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn encoded_record_len(values: &[Value]) -> Result<usize, DbError> {
    let mut length = RECORD_PREFIX;
    for value in values {
        length += encoded_value_len(value)?;
    }
    Ok(length)
}

pub(crate) fn encode_record(record: &StoredRecord) -> Result<Vec<u8>, DbError> {
    ensure_record_fits(&record.values)?;
    let mut bytes = Vec::with_capacity(encoded_record_len(&record.values)?);
    bytes.extend(record.row_id.to_le_bytes());
    push_link(&mut bytes, record.prev);
    push_link(&mut bytes, record.next);
    bytes.extend(u16_len(&record.values)?);
    for value in &record.values {
        encode_value(&mut bytes, value)?;
    }
    Ok(bytes)
}

pub(crate) fn decode_record(bytes: &[u8]) -> Result<StoredRecord, DbError> {
    if bytes.len() < RECORD_PREFIX {
        return Err(corrupt_file());
    }
    let row_id = u64::from_le_bytes(copy_array(bytes, 0)?);
    let prev = read_link(bytes, 8)?;
    let next = read_link(bytes, 14)?;
    let count = u16::from_le_bytes(copy_array(bytes, 20)?) as usize;
    let mut cursor = RECORD_PREFIX;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let (value, next_cursor) = decode_value(bytes, cursor)?;
        cursor = next_cursor;
        values.push(value);
    }
    if cursor != bytes.len() {
        return Err(corrupt_file());
    }
    Ok(StoredRecord {
        row_id,
        prev,
        next,
        values,
    })
}

pub(crate) fn patch_links(cell: &mut [u8], prev: Link, next: Link) -> Result<(), DbError> {
    if cell.len() < RECORD_PREFIX {
        return Err(corrupt_file());
    }
    cell[8..12].copy_from_slice(&prev.page.to_le_bytes());
    cell[12..14].copy_from_slice(&prev.slot.to_le_bytes());
    cell[14..18].copy_from_slice(&next.page.to_le_bytes());
    cell[18..20].copy_from_slice(&next.slot.to_le_bytes());
    Ok(())
}

/// Update the fixed head of a catalog cell. The name and columns stay put.
pub(crate) fn patch_catalog_head(
    cell: &mut [u8],
    next_row_id: u64,
    head: Link,
) -> Result<(), DbError> {
    if cell.len() < 18 {
        return Err(corrupt_file());
    }
    cell[4..12].copy_from_slice(&next_row_id.to_le_bytes());
    cell[12..16].copy_from_slice(&head.page.to_le_bytes());
    cell[16..18].copy_from_slice(&head.slot.to_le_bytes());
    Ok(())
}

pub(crate) fn encode_catalog(
    table_id: u32,
    next_row_id: u64,
    head: Link,
    name: &str,
    columns: &[Column],
) -> Result<Vec<u8>, DbError> {
    let mut bytes = Vec::new();
    bytes.extend(table_id.to_le_bytes());
    bytes.extend(next_row_id.to_le_bytes());
    bytes.extend(head.page.to_le_bytes());
    bytes.extend(head.slot.to_le_bytes());
    push_str(&mut bytes, name)?;
    let count = u16::try_from(columns.len()).map_err(|_| definition_too_big())?;
    bytes.extend(count.to_le_bytes());
    for column in columns {
        push_str(&mut bytes, column.name())?;
        bytes.push(type_code(column.data_type()));
    }
    if bytes.len() > MAX_RECORD_LEN {
        return Err(definition_too_big());
    }
    Ok(bytes)
}

pub(crate) struct CatalogEntry {
    pub table_id: u32,
    pub next_row_id: u64,
    pub head: Link,
    pub name: String,
    pub columns: Vec<Column>,
}

pub(crate) fn decode_catalog(bytes: &[u8]) -> Result<CatalogEntry, DbError> {
    let mut cursor = 0;
    let table_id = read_u32(bytes, &mut cursor)?;
    let next_row_id = read_u64(bytes, &mut cursor)?;
    let head = Link {
        page: read_u32(bytes, &mut cursor)?,
        slot: read_u16(bytes, &mut cursor)?,
    };
    let name = read_string(bytes, &mut cursor)?;
    let count = read_u16(bytes, &mut cursor)? as usize;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let column_name = read_string(bytes, &mut cursor)?;
        let code = *bytes.get(cursor).ok_or_else(corrupt_file)?;
        cursor += 1;
        columns.push(Column::new(column_name, data_type(code)?));
    }
    if cursor != bytes.len() || name.is_empty() {
        return Err(corrupt_file());
    }
    Ok(CatalogEntry {
        table_id,
        next_row_id,
        head,
        name,
        columns,
    })
}

fn definition_too_big() -> DbError {
    DbError::Storage(format!(
        "table definition exceeds the maximum record size of {MAX_RECORD_LEN} bytes"
    ))
}

fn encoded_value_len(value: &Value) -> Result<usize, DbError> {
    Ok(match value {
        Value::Null => 1,
        Value::Integer(_) => 9,
        Value::Boolean(_) => 2,
        Value::Text(text) => 1 + 4 + text_len(text)?,
    })
}

fn encode_value(bytes: &mut Vec<u8>, value: &Value) -> Result<(), DbError> {
    match value {
        Value::Null => bytes.push(TAG_NULL),
        Value::Integer(value) => {
            bytes.push(TAG_INTEGER);
            bytes.extend(value.to_le_bytes());
        }
        Value::Text(text) => {
            bytes.push(TAG_TEXT);
            let len = u32::try_from(text_len(text)?).map_err(|_| record_too_big())?;
            bytes.extend(len.to_le_bytes());
            bytes.extend(text.as_bytes());
        }
        Value::Boolean(value) => {
            bytes.push(TAG_BOOLEAN);
            bytes.push(u8::from(*value));
        }
    }
    Ok(())
}

fn decode_value(bytes: &[u8], cursor: usize) -> Result<(Value, usize), DbError> {
    let tag = *bytes.get(cursor).ok_or_else(corrupt_file)?;
    let mut cursor = cursor + 1;
    let value = match tag {
        TAG_NULL => Value::Null,
        TAG_INTEGER => {
            let array = copy_array::<8>(bytes, cursor)?;
            cursor += 8;
            Value::Integer(i64::from_le_bytes(array))
        }
        TAG_TEXT => {
            let len = u32::from_le_bytes(copy_array(bytes, cursor)?) as usize;
            cursor += 4;
            let end = cursor.checked_add(len).ok_or_else(corrupt_file)?;
            let text = bytes.get(cursor..end).ok_or_else(corrupt_file)?;
            cursor = end;
            let text = std::str::from_utf8(text).map_err(|_| corrupt_file())?;
            Value::Text(text.to_string())
        }
        TAG_BOOLEAN => {
            let byte = *bytes.get(cursor).ok_or_else(corrupt_file)?;
            cursor += 1;
            match byte {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                _ => return Err(corrupt_file()),
            }
        }
        _ => return Err(corrupt_file()),
    };
    Ok((value, cursor))
}

fn push_link(bytes: &mut Vec<u8>, link: Link) {
    bytes.extend(link.page.to_le_bytes());
    bytes.extend(link.slot.to_le_bytes());
}

fn read_link(bytes: &[u8], offset: usize) -> Result<Link, DbError> {
    Ok(Link {
        page: u32::from_le_bytes(copy_array(bytes, offset)?),
        slot: u16::from_le_bytes(copy_array(bytes, offset + 4)?),
    })
}

fn push_str(bytes: &mut Vec<u8>, text: &str) -> Result<(), DbError> {
    let len = u16::try_from(text.len()).map_err(|_| definition_too_big())?;
    bytes.extend(len.to_le_bytes());
    bytes.extend(text.as_bytes());
    Ok(())
}

fn read_string(bytes: &[u8], cursor: &mut usize) -> Result<String, DbError> {
    let len = read_u16(bytes, cursor)? as usize;
    let end = cursor.checked_add(len).ok_or_else(corrupt_file)?;
    let text = bytes.get(*cursor..end).ok_or_else(corrupt_file)?;
    *cursor = end;
    std::str::from_utf8(text)
        .map(|text| text.to_string())
        .map_err(|_| corrupt_file())
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, DbError> {
    let value = u16::from_le_bytes(copy_array(bytes, *cursor)?);
    *cursor += 2;
    Ok(value)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, DbError> {
    let value = u32::from_le_bytes(copy_array(bytes, *cursor)?);
    *cursor += 4;
    Ok(value)
}

fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, DbError> {
    let value = u64::from_le_bytes(copy_array(bytes, *cursor)?);
    *cursor += 8;
    Ok(value)
}

fn copy_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], DbError> {
    let end = offset.checked_add(N).ok_or_else(corrupt_file)?;
    let slice = bytes.get(offset..end).ok_or_else(corrupt_file)?;
    let mut array = [0u8; N];
    array.copy_from_slice(slice);
    Ok(array)
}

fn u16_len<T>(items: &[T]) -> Result<[u8; 2], DbError> {
    u16::try_from(items.len())
        .map(u16::to_le_bytes)
        .map_err(|_| record_too_big())
}

fn text_len(text: &str) -> Result<usize, DbError> {
    if text.len() > MAX_RECORD_LEN {
        Err(record_too_big())
    } else {
        Ok(text.len())
    }
}

fn record_too_big() -> DbError {
    DbError::Storage(format!(
        "row exceeds the maximum record size of {MAX_RECORD_LEN} bytes"
    ))
}

fn type_code(data_type: DataType) -> u8 {
    match data_type {
        DataType::Integer => TYPE_INTEGER,
        DataType::Text => TYPE_TEXT,
        DataType::Boolean => TYPE_BOOLEAN,
    }
}

fn data_type(code: u8) -> Result<DataType, DbError> {
    match code {
        TYPE_INTEGER => Ok(DataType::Integer),
        TYPE_TEXT => Ok(DataType::Text),
        TYPE_BOOLEAN => Ok(DataType::Boolean),
        _ => Err(corrupt_file()),
    }
}

/// One index definition stored in the index catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexCatalog {
    pub name: String,
    pub table_id: u32,
    pub ordinal: u16,
    pub column: String,
    pub root: u32,
}

const INDEX_KEY_INTEGER: u8 = 1;
const INDEX_FORMAT: u16 = 1;

pub(crate) fn encode_index(entry: &IndexCatalog) -> Result<Vec<u8>, DbError> {
    let name = entry.name.as_bytes();
    let column = entry.column.as_bytes();
    let name_len =
        u16::try_from(name.len()).map_err(|_| DbError::invalid("index name is too long"))?;
    let column_len =
        u16::try_from(column.len()).map_err(|_| DbError::invalid("column name is too long"))?;
    let mut bytes = Vec::with_capacity(2 + name.len() + 4 + 2 + 2 + column.len() + 4 + 1 + 2);
    bytes.extend(name_len.to_le_bytes());
    bytes.extend(name);
    bytes.extend(entry.table_id.to_le_bytes());
    bytes.extend(entry.ordinal.to_le_bytes());
    bytes.extend(column_len.to_le_bytes());
    bytes.extend(column);
    bytes.extend(entry.root.to_le_bytes());
    bytes.push(INDEX_KEY_INTEGER);
    bytes.extend(INDEX_FORMAT.to_le_bytes());
    if bytes.len() > MAX_RECORD_LEN {
        return Err(DbError::Storage(format!(
            "index definition exceeds the maximum record size of {MAX_RECORD_LEN} bytes"
        )));
    }
    Ok(bytes)
}

pub(crate) fn decode_index(bytes: &[u8]) -> Result<IndexCatalog, DbError> {
    if bytes.len() < 2 {
        return Err(corrupt_file());
    }
    let name_len = u16::from_le_bytes(copy_array(bytes, 0)?) as usize;
    let name_end = 2usize.checked_add(name_len).ok_or_else(corrupt_file)?;
    if bytes.len() < name_end + 4 + 2 + 2 {
        return Err(corrupt_file());
    }
    let name = std::str::from_utf8(&bytes[2..name_end])
        .map_err(|_| corrupt_file())?
        .to_string();
    let table_id = u32::from_le_bytes(copy_array(bytes, name_end)?);
    let ordinal = u16::from_le_bytes(copy_array(bytes, name_end + 4)?);
    let column_len_at = name_end + 6;
    let column_len = u16::from_le_bytes(copy_array(bytes, column_len_at)?) as usize;
    let column_at = column_len_at + 2;
    let column_end = column_at.checked_add(column_len).ok_or_else(corrupt_file)?;
    let tail = column_end.checked_add(4 + 1 + 2).ok_or_else(corrupt_file)?;
    if bytes.len() != tail {
        return Err(corrupt_file());
    }
    let column = std::str::from_utf8(&bytes[column_at..column_end])
        .map_err(|_| corrupt_file())?
        .to_string();
    let root = u32::from_le_bytes(copy_array(bytes, column_end)?);
    let key_type = bytes[column_end + 4];
    let format = u16::from_le_bytes(copy_array(bytes, column_end + 5)?);
    if key_type != INDEX_KEY_INTEGER || format != INDEX_FORMAT {
        return Err(DbError::Storage("unsupported index format".to_string()));
    }
    if name.is_empty() || column.is_empty() || table_id == 0 {
        return Err(corrupt_file());
    }
    Ok(IndexCatalog {
        name,
        table_id,
        ordinal,
        column,
        root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_null_and_scalar_values() {
        let values = vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Text(String::new()),
            Value::Text("snowman \u{2603}\0".to_string()),
            Value::Boolean(false),
            Value::Boolean(true),
        ];
        let record = StoredRecord {
            row_id: 42,
            prev: Link { page: 2, slot: 1 },
            next: NULL_LINK,
            values: values.clone(),
        };
        let bytes = encode_record(&record).unwrap();
        assert_eq!(bytes.len(), encoded_record_len(&values).unwrap());
        assert_eq!(decode_record(&bytes).unwrap(), record);

        let mut patched = bytes.clone();
        patch_links(&mut patched, NULL_LINK, Link { page: 4, slot: 7 }).unwrap();
        let decoded = decode_record(&patched).unwrap();
        assert_eq!(decoded.prev, NULL_LINK);
        assert_eq!(decoded.next, Link { page: 4, slot: 7 });
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn rejects_an_oversized_text_value_and_invalid_bytes() {
        let huge = "x".repeat(MAX_RECORD_LEN);
        let error = ensure_record_fits(&[Value::Text(huge)]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("row exceeds the maximum record size of {MAX_RECORD_LEN} bytes")
        );

        let error = decode_record(&[0, 1, 2]).unwrap_err();
        assert_eq!(error.to_string(), "database file is corrupt");
        let mut boolean = encode_record(&StoredRecord {
            row_id: 1,
            prev: NULL_LINK,
            next: NULL_LINK,
            values: vec![Value::Boolean(true)],
        })
        .unwrap();
        *boolean.last_mut().unwrap() = 2;
        assert_eq!(
            decode_record(&boolean).unwrap_err().to_string(),
            "database file is corrupt"
        );
    }

    #[test]
    fn round_trips_a_catalog_entry() {
        let columns = vec![
            Column::new("id", DataType::Integer),
            Column::new("name", DataType::Text),
            Column::new("active", DataType::Boolean),
        ];
        let bytes = encode_catalog(7, 9, Link { page: 3, slot: 1 }, "Users", &columns).unwrap();
        let entry = decode_catalog(&bytes).unwrap();
        assert_eq!(entry.table_id, 7);
        assert_eq!(entry.next_row_id, 9);
        assert_eq!(entry.head, Link { page: 3, slot: 1 });
        assert_eq!(entry.name, "Users");
        assert_eq!(entry.columns, columns);
    }
}
