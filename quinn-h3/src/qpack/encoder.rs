// This is only here because qpack is new and quinn no uses it yet.
// TODO remove allow dead code
#![allow(dead_code)]

use bytes::BufMut;
use std::borrow::Cow;
use std::cmp;
use std::collections::VecDeque;
use std::io::Cursor;

use quinn_proto::coding::{BufMutExt, Codec};

use super::table::{
    DynamicInsertionResult, DynamicLookupResult, DynamicTable, DynamicTableEncoder,
    DynamicTableError, HeaderField, StaticTable,
};
use super::vas::VirtualAddressSpace;

use super::bloc::{
    HeaderBlocField, HeaderPrefix, Indexed, IndexedWithPostBase, Literal, LiteralWithNameRef,
    LiteralWithPostBaseNameRef,
};
use super::stream::{
    Duplicate, DynamicTableSizeUpdate, InsertWithNameRef, InsertWithoutNameRef, InstructionType,
    TableSizeSync,
};

use super::prefix_string::Error as StringError;

pub fn encode<W: BufMut>(
    table: &mut DynamicTableEncoder,
    bloc: &mut W,
    encoder: &mut W,
    fields: &[HeaderField],
) -> Result<usize, Error> {
    let mut required_ref = 0;
    let mut bloc_buf = Vec::new();

    for field in fields {
        if let Some(reference) = encode_field(table, &mut bloc_buf, encoder, field)? {
            required_ref = cmp::max(required_ref, reference);
        }
    }

    HeaderPrefix::new(
        required_ref,
        table.base(),
        table.total_inserted(),
        table.max_mem_size(),
    )
    .encode(bloc);

    bloc.put(bloc_buf);

    Ok(required_ref)
}

fn encode_field<W: BufMut>(
    table: &mut DynamicTableEncoder,
    bloc: &mut Vec<u8>,
    encoder: &mut W,
    field: &HeaderField,
) -> Result<Option<usize>, Error> {
    if let Some(index) = StaticTable::find(field) {
        Indexed::Static(index).encode(bloc);
        return Ok(None);
    }

    if let DynamicLookupResult::Relative { index, absolute } = table.find(field) {
        Indexed::Dynamic(index).encode(bloc);
        Ok(Some(absolute))
    } else if table.can_insert(&field) {
        encode_field_insert(table, bloc, encoder, field)
    } else {
        encode_field_literal(table, bloc, field)
    }
}

fn encode_field_insert<W: BufMut>(
    table: &mut DynamicTableEncoder,
    bloc: &mut Vec<u8>,
    encoder: &mut W,
    field: &HeaderField,
) -> Result<Option<usize>, Error> {
    if let Some(static_index) = StaticTable::find_name(&field.name) {
        match table.insert_static(&field)? {
            DynamicInsertionResult::Duplicated {
                relative,
                postbase,
                absolute,
            } => {
                Duplicate(relative).encode(encoder);
                IndexedWithPostBase(postbase).encode(bloc);
                return Ok(Some(absolute));
            }
            DynamicInsertionResult::Inserted { postbase, absolute } => {
                InsertWithNameRef::new_static(static_index, field.value.clone()).encode(encoder)?;
                IndexedWithPostBase(postbase).encode(bloc);
                return Ok(Some(absolute));
            }
            _ => unreachable!(),
        }
    }

    let reference = match table.insert(&field)? {
        DynamicInsertionResult::Inserted { postbase, absolute } => {
            InsertWithoutNameRef::new(field.name.clone(), field.value.clone()).encode(encoder)?;
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::Duplicated {
            relative,
            postbase,
            absolute,
        } => {
            Duplicate(relative).encode(encoder);
            Indexed::Dynamic(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::InsertedWithNameRef {
            postbase,
            relative,
            absolute,
        } => {
            InsertWithNameRef::new_dynamic(relative, field.value.clone()).encode(encoder)?;
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
    };
    Ok(reference)
}

pub fn encode_field_literal<W: BufMut>(
    table: &DynamicTableEncoder,
    bloc: &mut W,
    field: &HeaderField,
) -> Result<Option<usize>, Error> {
    if let Some(static_index) = StaticTable::find_name(&field.name) {
        LiteralWithNameRef::new_static(static_index, field.value.clone()).encode(bloc)?;
        return Ok(None);
    }

    let reference = match table.find_name(&field.name) {
        DynamicLookupResult::Relative { index, absolute } => {
            LiteralWithNameRef::new_dynamic(index, field.value.clone()).encode(bloc)?;
            Some(absolute)
        }
        DynamicLookupResult::PostBase { index, absolute } => {
            LiteralWithPostBaseNameRef::new(index, field.value.clone()).encode(bloc)?;
            Some(absolute)
        }
        DynamicLookupResult::NotFound => {
            Literal::new(field.name.clone(), field.value.clone()).encode(bloc)?;
            None
        }
    };
    Ok(reference)
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Insertion(DynamicTableError),
    InvalidString(StringError),
}

impl From<DynamicTableError> for Error {
    fn from(e: DynamicTableError) -> Self {
        Error::Insertion(e)
    }
}

impl From<StringError> for Error {
    fn from(e: StringError) -> Self {
        Error::InvalidString(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qpack::table::dynamic::SETTINGS_HEADER_TABLE_SIZE_DEFAULT as TABLE_SIZE;

    fn check_encode_field(
        init_fields: &[HeaderField],
        field: &[HeaderField],
        check: &Fn(&mut Cursor<&mut Vec<u8>>, &mut Cursor<&mut Vec<u8>>),
    ) {
        check_encode_field_table(&mut DynamicTable::new(), init_fields, field, check);
    }

    fn check_encode_field_table(
        table: &mut DynamicTable,
        init_fields: &[HeaderField],
        field: &[HeaderField],
        check: &Fn(&mut Cursor<&mut Vec<u8>>, &mut Cursor<&mut Vec<u8>>),
    ) {
        for field in init_fields {
            table.inserter().put_field(field.clone()).unwrap();
        }

        let mut encoder = Vec::new();
        let mut bloc = Vec::new();
        let mut enc_table = table.encoder();

        for field in field {
            encode_field(&mut enc_table, &mut bloc, &mut encoder, field).unwrap();
        }

        let mut read_bloc = Cursor::new(&mut bloc);
        let mut read_encoder = Cursor::new(&mut encoder);
        check(&mut read_bloc, &mut read_encoder);
    }

    #[test]
    fn encode_static() {
        let field = HeaderField::new(":method", "GET");
        check_encode_field(&[], &[field], &|mut b, e| {
            assert_eq!(Indexed::decode(&mut b), Ok(Indexed::Static(17)));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_static_nameref() {
        let field = HeaderField::new("location", "/bar");
        check_encode_field(&[], &[field], &|mut b, mut e| {
            assert_eq!(
                IndexedWithPostBase::decode(&mut b),
                Ok(IndexedWithPostBase(0))
            );
            assert_eq!(
                InsertWithNameRef::decode(&mut e),
                Ok(Some(InsertWithNameRef::new_static(12, "/bar")))
            );
        });
    }

    #[test]
    fn encode_static_nameref_indexed_in_dynamic() {
        let field = HeaderField::new("location", "/bar");
        check_encode_field(&[field.clone()], &[field], &|mut b, e| {
            assert_eq!(Indexed::decode(&mut b), Ok(Indexed::Dynamic(0)));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_dynamic_insert() {
        let field = HeaderField::new("foo", "bar");
        check_encode_field(&[], &[field], &|mut b, mut e| {
            assert_eq!(
                IndexedWithPostBase::decode(&mut b),
                Ok(IndexedWithPostBase(0))
            );
            assert_eq!(
                InsertWithoutNameRef::decode(&mut e),
                Ok(Some(InsertWithoutNameRef::new("foo", "bar")))
            );
        });
    }

    #[test]
    fn encode_dynamic_insert_nameref() {
        let field = HeaderField::new("foo", "bar");
        check_encode_field(
            &[field.clone(), HeaderField::new("baz", "bar")],
            &[field.with_value("quxx")],
            &|mut b, mut e| {
                assert_eq!(
                    IndexedWithPostBase::decode(&mut b),
                    Ok(IndexedWithPostBase(0))
                );
                assert_eq!(
                    InsertWithNameRef::decode(&mut e),
                    Ok(Some(InsertWithNameRef::new_dynamic(1, "quxx")))
                );
            },
        );
    }

    #[test]
    fn encode_literal() {
        let mut table = DynamicTable::new();
        table.set_max_mem_size(0).unwrap();
        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(&mut table, &[], &[field], &|mut b, e| {
            assert_eq!(Literal::decode(&mut b), Ok(Literal::new("foo", "bar")));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_literal_nameref() {
        let mut table = DynamicTable::new();
        table.set_max_mem_size(63).unwrap();
        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(
            &mut table,
            &[field.clone()],
            &[field.with_value("quxx")],
            &|mut b, e| {
                assert_eq!(
                    LiteralWithNameRef::decode(&mut b),
                    Ok(LiteralWithNameRef::new_dynamic(0, "quxx"))
                );
                assert_eq!(e.get_ref().len(), 0);
            },
        );
    }

    #[test]
    fn encode_literal_postbase_nameref() {
        let mut table = DynamicTable::new();
        table.set_max_mem_size(63).unwrap();
        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(
            &mut table,
            &[],
            &[field.clone(), field.with_value("quxx")],
            &|mut b, mut e| {
                assert_eq!(
                    IndexedWithPostBase::decode(&mut b),
                    Ok(IndexedWithPostBase(0))
                );
                assert_eq!(
                    LiteralWithPostBaseNameRef::decode(&mut b),
                    Ok(LiteralWithPostBaseNameRef::new(0, "quxx"))
                );
                assert_eq!(
                    InsertWithoutNameRef::decode(&mut e),
                    Ok(Some(InsertWithoutNameRef::new("foo", "bar")))
                );
            },
        );
    }

    #[test]
    fn encode_with_header_bloc() {
        let mut table = DynamicTable::new();

        for idx in 1..5 {
            table
                .inserter()
                .put_field(HeaderField::new(
                    format!("foo{}", idx),
                    format!("bar{}", idx),
                ))
                .unwrap();
        }

        let mut encoder = Vec::new();
        let mut bloc = Vec::new();

        let fields = [
            HeaderField::new(":method", "GET"),
            HeaderField::new("foo1", "bar1"),
            HeaderField::new("foo3", "new bar3"),
            HeaderField::new(":method", "staticnameref"),
            HeaderField::new("newfoo", "newbar"),
        ];

        assert_eq!(
            encode(&mut table.encoder(), &mut bloc, &mut encoder, &fields),
            Ok(7)
        );

        let mut read_bloc = Cursor::new(&mut bloc);
        let mut read_encoder = Cursor::new(&mut encoder);

        assert_eq!(
            InsertWithNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithNameRef::new_dynamic(1, "new bar3")))
        );
        assert_eq!(
            InsertWithNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithNameRef::new_static(
                StaticTable::find_name(&b":method"[..]).unwrap(),
                "staticnameref"
            )))
        );
        assert_eq!(
            InsertWithoutNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithoutNameRef::new("newfoo", "newbar")))
        );

        assert_eq!(
            HeaderPrefix::decode(&mut read_bloc)
                .unwrap()
                .get(7, TABLE_SIZE),
            Ok((7, 4))
        );
        assert_eq!(Indexed::decode(&mut read_bloc), Ok(Indexed::Static(17)));
        assert_eq!(Indexed::decode(&mut read_bloc), Ok(Indexed::Dynamic(3)));
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(0))
        );
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(1))
        );
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(2))
        );
        assert_eq!(read_bloc.get_ref().len() as u64, read_bloc.position());
    }
}