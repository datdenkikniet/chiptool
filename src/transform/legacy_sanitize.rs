use inflections::Inflect;
use serde::{Deserialize, Serialize};

use super::{map_names, NameKind, IR};

#[derive(Debug, Serialize, Deserialize)]
pub struct LegacySanitize {}

impl LegacySanitize {
    pub fn run(&self, ir: &mut IR) -> anyhow::Result<()> {
        map_names(ir, |k, p| match k {
            NameKind::Device => *p = sanitize_path(p),
            NameKind::DevicePeripheral => *p = to_sanitized_constant_case(p),
            NameKind::DeviceInterrupt => *p = to_sanitized_constant_case(p),
            NameKind::Block => *p = sanitize_path(p),
            NameKind::Fieldset => *p = sanitize_path(p),
            NameKind::Enum => *p = sanitize_path(p),
            NameKind::BlockItem => *p = to_sanitized_snake_case(p),
            NameKind::Field => *p = to_sanitized_snake_case(p),
            NameKind::EnumVariant => *p = to_sanitized_constant_case(p),
        });

        // After sanitizing names, merge duplicate enum variants with the same name and value
        for (_, enumm) in ir.enums.iter_mut() {
            super::sanitize::merge_duplicate_variants(enumm);
            // rename duplicate enum variants with the same name but different values
            super::sanitize::rename_duplicate_variants(enumm);
        }

        Ok(())
    }
}

fn sanitize_path(p: &str) -> String {
    let v = p.split("::").collect::<Vec<_>>();
    let len = v.len();
    v.into_iter()
        .enumerate()
        .map(|(i, s)| {
            if i == len - 1 {
                to_sanitized_pascal_case(s)
            } else {
                to_sanitized_snake_case(s)
            }
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn to_sanitized_snake_case(str: &str) -> String {
    super::sanitize::sanitize_ident(str.to_snake_case())
}

fn to_sanitized_constant_case(str: &str) -> String {
    super::sanitize::sanitize_ident(str.to_constant_case())
}

fn to_sanitized_pascal_case(str: &str) -> String {
    super::sanitize::sanitize_ident(str.to_pascal_case())
}
