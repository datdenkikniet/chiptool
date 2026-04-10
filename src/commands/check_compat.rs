use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use regex::Regex;

use crate::commands::{apply_transform, load_svd};
use crate::ir::{Access, Array, BitOffset, Block, BlockItem, Field, FieldSet, IR};
use crate::svd2ir::{self, NamespaceMode};
use crate::transform::common::CheckLevel;
use crate::transform::sanitize::Sanitize;

#[derive(Parser)]
pub struct CheckCompat {
    /// SVD file path.
    #[clap(long)]
    pub svd: PathBuf,
    /// Transforms file paths.
    #[clap(long)]
    pub transform: Vec<PathBuf>,
    /// Namespaces added to each extracted peripheral.
    #[clap(long, value_enum, default_value = "block-with-regs-vals")]
    pub namespaces: NamespaceMode,

    #[clap(long, short)]
    pub main: Option<String>,

    #[clap(long)]
    pub exclude: Option<String>,

    pub include: String,
}

#[derive(Clone, Debug)]
enum Item {
    Block(String, Block),
    Fieldset(String, FieldSet),
}

impl Item {
    fn name(&self) -> &str {
        match self {
            Item::Block(name, _) | Item::Fieldset(name, _) => name,
        }
    }
}

impl core::fmt::Display for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Item::Block(name, _) => write!(f, "block '{name}'"),
            Item::Fieldset(name, _) => write!(f, "fieldset '{name}'"),
        }
    }
}

fn find_all(ir: &IR, include: Regex, exclude: Option<Regex>) -> impl Iterator<Item = Item> + '_ {
    let block_include = include.clone();
    let block_exclude = exclude.clone();

    let blocks = ir.blocks.iter().filter_map(move |(k, v)| {
        let include = block_include.is_match(k);
        let exclude = block_exclude.as_ref().is_some_and(|v| v.is_match(k));
        (include && !exclude)
            .then(|| v.clone())
            .map(|v| Item::Block(k.clone(), v))
    });

    let fieldset_include = include.clone();
    let fieldset_exclude = exclude.clone();
    let fieldsets = ir.fieldsets.iter().filter_map(move |(k, v)| {
        let include = fieldset_include.is_match(k);
        let exclude = fieldset_exclude.as_ref().is_some_and(|v| v.is_match(k));
        (include && !exclude)
            .then(|| v.clone())
            .map(|v| Item::Fieldset(k.clone(), v))
    });

    blocks.chain(fieldsets)
}

fn find(ir: &IR, name: &str) -> Option<Item> {
    find_all(ir, Regex::new(&format!("^{}$", name)).unwrap(), None).next()
}

fn get_list<'a>(
    ir: &'a IR,
    check: &CheckCompat,
) -> anyhow::Result<(Item, impl Iterator<Item = Item> + 'a)> {
    let include = Regex::new(&format!("^{}$", check.include)).context("invalid include regex")?;
    let exclude = check
        .exclude
        .as_ref()
        .map(|v| Regex::new(&format!("^{}$", v)).context("invalid exclude regex"))
        .transpose()?;

    let mut all = find_all(ir, include, exclude);

    let main = if let Some(main) = &check.main {
        find(ir, main).context(format!("Failed to find main"))?
    } else {
        all.next().context("found no items to include")?
    };

    Ok((main, all))
}

pub fn check_compat(args: CheckCompat) -> anyhow::Result<()> {
    let svd = load_svd(&args.svd)?;
    let mut ir = svd2ir::convert_svd(&svd, args.namespaces)?;

    Sanitize {}.run(&mut ir)?;

    for transform in &args.transform {
        apply_transform(&mut ir, transform)?;
    }

    let (main, rest) = get_list(&ir, &args)?;
    let rest: Vec<_> = rest.collect();

    log::info!("Found main: {}", main.name());

    for rest in &rest {
        log::info!("Found additional: {}", rest.name());
    }

    let mut ok = true;

    for rest in rest {
        match (&main, &rest) {
            (Item::Block(b1n, b1), Item::Block(b2n, b2)) => {
                let block_errors = block_compat(&ir, b1, b2, CheckLevel::Descriptions);

                for error in &block_errors {
                    log::error!("{b1n} <=> {b2n}: {error}");
                }

                ok &= block_errors.is_empty();
            }
            (Item::Fieldset(f1n, f1), Item::Fieldset(f2n, f2)) => {
                let fs_errors = fieldset_compat(f1, f2, CheckLevel::Descriptions);

                for error in &fs_errors {
                    log::error!("{f1n} <=> {f2n}: {error}");
                }

                ok &= fs_errors.is_empty();
            }
            (_1, _2) => log::error!("{} '{}' != {} '{}'", main, main.name(), rest, rest.name()),
        }
    }

    if ok {
        Ok(())
    } else {
        Err(anyhow::format_err!("Incompatibilities were detected",))
    }
}

enum BlockError {
    Description(Option<String>, Option<String>),
    Extends(Option<String>, Option<String>),
    LhsMissingItem(String),
    RhsMissingItem(String),
    NameMismatch {
        lhs: String,
        rhs: String,
        byte_offset: u32,
    },
    Item(String, u32, BlockItemError),
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::Description(l, r) => write!(f, "description mismatch: '{l:?}' != '{r:?}'"),
            BlockError::Extends(l, r) => write!(f, "extends mismatch: '{l:?}' != '{r:?}'"),
            BlockError::Item(name, offset, error) => {
                write!(f, "inner item {name} at offset {offset}: {error}")
            }
            BlockError::LhsMissingItem(i) => write!(f, "lhs is missing inner item '{i}'"),
            BlockError::RhsMissingItem(i) => write!(f, "rhs is missing inner item '{i}'"),
            BlockError::NameMismatch {
                lhs,
                rhs,
                byte_offset,
            } => write!(
                f,
                "name mismatch for item at offset {byte_offset}: '{lhs}' != '{rhs}'"
            ),
        }
    }
}

fn block_compat(ir: &IR, left: &Block, right: &Block, level: CheckLevel) -> Vec<BlockError> {
    let mut mismatches = Vec::new();

    if level >= CheckLevel::Descriptions && left.description != right.description {
        mismatches.push(BlockError::Description(
            left.description.clone(),
            right.description.clone(),
        ));
    }

    if left.extends != right.extends {
        mismatches.push(BlockError::Extends(
            left.extends.clone(),
            right.extends.clone(),
        ));
    }

    for left in left.items.iter() {
        let Some(right) = right
            .items
            .iter()
            .find(|v| v.byte_offset == left.byte_offset)
        else {
            mismatches.push(BlockError::RhsMissingItem(left.name.clone()));
            continue;
        };

        if level >= CheckLevel::Names && left.name != right.name {
            mismatches.push(BlockError::NameMismatch {
                lhs: left.name.clone(),
                rhs: right.name.clone(),
                byte_offset: left.byte_offset.clone(),
            })
        }

        mismatches.extend(
            block_item_compat(ir, left, right, level)
                .into_iter()
                .map(|v| BlockError::Item(left.name.clone(), left.byte_offset, v)),
        );
    }

    for right in right.items.iter() {
        if left
            .items
            .iter()
            .find(|v| v.byte_offset == right.byte_offset)
            .is_none()
        {
            mismatches.push(BlockError::LhsMissingItem(right.name.clone()));
            continue;
        };
    }

    mismatches
}

fn fmt_access(access: &Access) -> &str {
    match access {
        Access::ReadWrite => "RW",
        Access::Read => "RO",
        Access::Write => "WO",
    }
}

enum BlockItemError {
    Description(Option<String>, Option<String>),
    ArrayXor(bool, bool),
    Array(ArrayError),
    InnerXor(bool),
    RegisterAccessMismatch(Access, Access),
    RegisterSizeMismatch(u32, u32),
    FieldSetXor(bool, bool),
    FieldSet(FieldSetError),
    Block(Box<BlockError>),
}

impl std::fmt::Display for BlockItemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockItemError::Description(l, r) => {
                write!(f, "description mismatch: '{l:?}' != '{r:?}'")
            }
            BlockItemError::ArrayXor(a, _b) => {
                if *a {
                    write!(f, "array mismatch: lhs is array, rhs is not")
                } else {
                    write!(f, "array mismatch: lhs is not array, rhs is")
                }
            }
            BlockItemError::Array(e) => write!(f, "array mismatch: {e}"),
            BlockItemError::InnerXor(true) => {
                write!(
                    f,
                    "inner type mismatch: lhs is nested block, rhs is register"
                )
            }
            BlockItemError::InnerXor(false) => {
                write!(
                    f,
                    "inner type mismatch: lhs is register, rhs is nested block"
                )
            }
            BlockItemError::RegisterAccessMismatch(left, right) => {
                write!(
                    f,
                    "register access mismatch: {} != {}",
                    fmt_access(left),
                    fmt_access(right)
                )
            }
            BlockItemError::RegisterSizeMismatch(left, right) => {
                write!(f, "register size mismatch: {left} != {right}")
            }
            BlockItemError::FieldSetXor(a, _b) => {
                if *a {
                    write!(f, "fieldset mismatch: lhs has fieldset, rhs does not")
                } else {
                    write!(f, "fieldset mismatch: lhs doesn't have fieldset, rhs does")
                }
            }
            BlockItemError::FieldSet(e) => write!(f, "field set mismatch: {e}"),
            BlockItemError::Block(e) => write!(f, "inner block mismatch: {e}"),
        }
    }
}

fn block_item_compat(
    ir: &IR,
    left: &BlockItem,
    right: &BlockItem,
    level: CheckLevel,
) -> Vec<BlockItemError> {
    assert_eq!(left.byte_offset, right.byte_offset);
    let mut mismatches = Vec::new();

    if level >= CheckLevel::Descriptions && left.description != right.description {
        mismatches.push(BlockItemError::Description(
            left.description.clone(),
            right.description.clone(),
        ));
    }

    match (left.array.as_ref(), right.array.as_ref()) {
        (None, None) => {}
        (Some(a1), Some(a2)) => {
            mismatches.extend(array_compat(a1, a2).into_iter().map(BlockItemError::Array))
        }
        (l, r) => mismatches.push(BlockItemError::ArrayXor(l.is_some(), r.is_some())),
    }

    use crate::ir::BlockItemInner::*;
    match (&left.inner, &right.inner) {
        (Block(left), Block(right)) => {
            let left = ir
                .blocks
                .get(&left.block)
                .expect("LHS to have existing block");
            let right = ir
                .blocks
                .get(&right.block)
                .expect("RHS to have existing block");

            mismatches.extend(
                block_compat(ir, left, right, level)
                    .into_iter()
                    .map(|e| BlockItemError::Block(Box::new(e))),
            );
        }
        (Register(left), Register(right)) => {
            if left.access != right.access {
                mismatches.push(BlockItemError::RegisterAccessMismatch(
                    left.access.clone(),
                    right.access.clone(),
                ));
            }

            if left.bit_size != right.bit_size {
                mismatches.push(BlockItemError::RegisterSizeMismatch(
                    left.bit_size,
                    right.bit_size,
                ));
            }

            match (left.fieldset.as_ref(), right.fieldset.as_ref()) {
                (Some(left), Some(right)) => {
                    let left = ir.fieldsets.get(left).expect("LHS has existing fieldset");
                    let right = ir.fieldsets.get(right).expect("RHS has existing fieldset");

                    mismatches.extend(
                        fieldset_compat(left, right, level)
                            .into_iter()
                            .map(BlockItemError::FieldSet),
                    );
                }
                (None, None) => {}
                (left, right) => {
                    mismatches.push(BlockItemError::FieldSetXor(left.is_some(), right.is_some()));
                }
            }
        }
        (left, _) => mismatches.push(BlockItemError::InnerXor(matches!(left, Block(_)))),
    }

    mismatches
}

enum FieldSetError {
    Bitsize(u32, u32),
    Extends(Option<String>, Option<String>),
    Field {
        lhs_name: String,
        bit_size: u32,
        bit_offset: BitOffset,
        error: FieldError,
    },
    LhsMissingfield(String),
    RhsMissingField(String),
    NameMismatch {
        lhs: String,
        rhs: String,
        bit_size: u32,
        bit_offset: BitOffset,
    },
    Description(Option<String>, Option<String>),
}

impl std::fmt::Display for FieldSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldSetError::Bitsize(s1, s2) => {
                write!(f, "size mismatch: {} != {}", s1, s2)
            }
            FieldSetError::Extends(e1, e2) => write!(f, "extends mismatch: {:?} != {:?}", e1, e2),
            FieldSetError::Field {
                lhs_name,
                bit_size,
                bit_offset,
                error,
            } => write!(
                f,
                "{lhs_name} at offset {} with size {bit_size}: {error}",
                bit_offset.min_offset()
            ),
            FieldSetError::LhsMissingfield(field) => write!(f, "lhs is missing field '{field}'"),
            FieldSetError::RhsMissingField(field) => write!(f, "rhs is missing field '{field}'"),
            FieldSetError::NameMismatch {
                lhs,
                rhs,
                bit_size,
                bit_offset,
            } => write!(
                f,
                "field at offset {} with size {} has name mismatch: '{lhs}' != '{rhs}'",
                bit_offset.min_offset(),
                bit_size
            ),
            FieldSetError::Description(l, r) => {
                write!(f, "description mismatch: '{l:?}' != '{r:?}'")
            }
        }
    }
}

fn fieldset_compat(left: &FieldSet, right: &FieldSet, level: CheckLevel) -> Vec<FieldSetError> {
    let mut mismatches = Vec::new();

    if left.extends != right.extends {
        mismatches.push(FieldSetError::Extends(
            left.extends.clone(),
            right.extends.clone(),
        ));
    }

    if left.bit_size != right.bit_size {
        mismatches.push(FieldSetError::Bitsize(left.bit_size, right.bit_size));
    }

    if level >= CheckLevel::Descriptions && left.description != right.description {
        mismatches.push(FieldSetError::Description(
            left.description.clone(),
            right.description.clone(),
        ));
    }

    for left in &left.fields {
        let Some(right) = right
            .fields
            .iter()
            .find(|f| f.bit_offset == left.bit_offset && f.bit_size == left.bit_size)
        else {
            mismatches.push(FieldSetError::RhsMissingField(left.name.clone()));
            continue;
        };

        if level >= CheckLevel::Names && left.name != right.name {
            mismatches.push(FieldSetError::NameMismatch {
                lhs: left.name.clone(),
                rhs: right.name.clone(),
                bit_size: left.bit_size,
                bit_offset: left.bit_offset.clone(),
            })
        }

        mismatches.extend(field_compat(&left, &right, level).into_iter().map(|error| {
            FieldSetError::Field {
                lhs_name: left.name.clone(),
                bit_offset: left.bit_offset.clone(),
                bit_size: left.bit_size,
                error,
            }
        }));
    }

    for right in right.fields.iter() {
        if left
            .fields
            .iter()
            .find(|f| f.bit_offset == right.bit_offset && f.bit_size == right.bit_size)
            .is_none()
        {
            mismatches.push(FieldSetError::LhsMissingfield(right.name.clone()));
            continue;
        };
    }

    mismatches
}

enum FieldError {
    Array(ArrayError),
    ArrayXor(bool, bool),
    Description(Option<String>, Option<String>),
    Enum(Option<String>, Option<String>),
}

impl core::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldError::ArrayXor(a1, _b) => {
                if *a1 {
                    write!(f, "array types mismatch: lhs is an array, rhs is not")
                } else {
                    write!(f, "array types mismatch: lhs is not an array, rhs is")
                }
            }
            FieldError::Array(e) => write!(f, "{}", e),
            FieldError::Description(l, r) => write!(f, "description mismatch: '{l:?}' != '{r:?}'"),
            FieldError::Enum(l, r) => write!(f, "enum mismatch: '{l:?}' != '{r:?}'"),
        }
    }
}

fn field_compat(left: &Field, right: &Field, level: CheckLevel) -> Vec<FieldError> {
    assert_eq!(left.bit_offset, right.bit_offset);
    assert_eq!(left.bit_size, right.bit_size);

    let mut mismatches = Vec::new();

    match (left.array.as_ref(), right.array.as_ref()) {
        (Some(a1), Some(a2)) => {
            mismatches.extend(array_compat(a1, a2).into_iter().map(FieldError::Array))
        }
        (None, None) => {}
        (a1, a2) => mismatches.push(FieldError::ArrayXor(a1.is_some(), a2.is_some())),
    }

    if level >= CheckLevel::Descriptions && left.description != right.description {
        mismatches.push(FieldError::Description(
            left.description.clone(),
            right.description.clone(),
        ));
    }

    if left.enumm != right.enumm {
        mismatches.push(FieldError::Enum(left.enumm.clone(), right.enumm.clone()));
    }

    mismatches
}

enum ArrayError {
    Xor(Array, Array),
    Len(usize, usize),
    Stride(u32, u32),
}

impl core::fmt::Display for ArrayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArrayError::Xor(Array::Regular(_), Array::Cursed(_)) => {
                write!(f, "array type mismatch: lhs is regular, rhs is cursed")
            }
            ArrayError::Xor(Array::Cursed(_), Array::Regular(_)) => {
                write!(f, "array type mismatch: lhs is cursed, rhs is regular")
            }
            ArrayError::Xor(..) => unreachable!(),
            ArrayError::Len(l1, l2) => write!(f, "array len mismatch: {} != {}", l1, l2),
            ArrayError::Stride(s1, s2) => write!(f, "stride mismatch: {} != {}", s1, s2),
        }
    }
}

fn array_compat(left: &Array, right: &Array) -> Vec<ArrayError> {
    let mut mismatches = Vec::new();

    if left.len() != right.len() {
        mismatches.push(ArrayError::Len(left.len(), right.len()));
    }

    match (left, right) {
        (Array::Regular(a1), Array::Regular(a2)) => {
            if a1.stride != a2.stride {
                mismatches.push(ArrayError::Stride(a1.stride, a2.stride));
            }
        }
        (Array::Cursed(_), Array::Cursed(_)) => {}
        (a1, a2) => mismatches.push(ArrayError::Xor(a1.clone(), a2.clone())),
    }

    mismatches
}
