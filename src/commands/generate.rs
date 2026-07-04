use crate::commands::{
    apply_transform, clean_up_ir, get_generate_opts, load_svd, GenerateShared, NamespaceMode,
};
use crate::ir::{BlockItemInner, IR};
use crate::{generate, svd2ir};
use anyhow::{Context, Result};
use clap::Parser;
use proc_macro2::{Ident, Span};
use quote::quote;
use std::fs;
use std::fs::File;
use std::path::PathBuf;

/// Generate a PAC directly from a SVD
#[derive(Parser)]
pub struct Generate {
    /// SVD file path.
    #[clap(long)]
    pub svd: PathBuf,
    /// Transforms file paths.
    #[clap(long)]
    pub transform: Vec<PathBuf>,
    /// Namespaces added to each extracted peripheral.
    #[clap(long, value_enum, default_value = "block-with-regs-vals")]
    pub namespaces: NamespaceMode,
    #[clap(flatten)]
    pub gen_shared: GenerateShared,

    /// Output YAML path for the whole IR. Useful for debugging
    #[clap(long)]
    pub debug_ir_output: Option<PathBuf>,
    /// Output directory of the PAC files.
    #[clap(long)]
    pub output: Option<PathBuf>,
}

pub fn generate(args: Generate) -> Result<()> {
    let svd =
        load_svd(&args.svd).with_context(|| format!("loading svd at {}", args.svd.display()))?;

    let mut ir = svd2ir::convert_svd(&svd, args.namespaces)
        .with_context(|| format!("converting svd at {}", args.svd.display()))?;

    clean_up_ir(&mut ir)?;

    for transform in args.transform {
        apply_transform(&mut ir, transform)?;
    }

    println!("use super::*;");
    for (name, _) in ir.fieldsets.iter() {
        for_one_mux(&name, &ir);
    }

    if let Some(path) = args.debug_ir_output {
        let f = File::create(&path)
            .with_context(|| format!("creating IR output yaml at {}", path.display()))?;
        serde_yaml::to_writer(f, &ir)
            .with_context(|| format!("writing IR output yaml at {}", path.display()))?;
    }
    let generate_opts = get_generate_opts(args.gen_shared)?;

    let output = if let Some(output) = args.output {
        output
    } else {
        std::env::current_dir()?
    };

    let items = generate::render(&ir, &generate_opts).unwrap();
    fs::write(output.join("lib.rs"), items.to_string())?;

    let device_x = generate::render_device_x(&ir, ir.devices.values().next().unwrap())?;
    fs::write(output.join("device.x"), device_x)?;

    Ok(())
}

#[derive(Debug, Clone)]
struct AltMode {
    mux_enum: String,
    mux_variant: String,
    daisy: Option<Daisy>,
}

#[derive(Debug, Clone)]
pub struct Daisy {
    fs_name: String,
    enumm: String,
    variant: String,
}

fn for_one_mux(fs_name: &str, ir: &IR) {
    let Ok([instance, _, reg]) = <[&str; 3]>::try_from(fs_name.splitn(3, "::").collect::<Vec<_>>())
    else {
        return;
    };

    let Some(struct_name) = reg.strip_prefix("SwMuxCtlPad") else {
        return;
    };

    let (iomuxc_block_path, iomuxc_block_name) = match instance.to_lowercase().as_str() {
        "iomuxc" => ("iomuxc::Iomuxc", "IOMUXC"),
        "iomuxc_aon" => ("iomuxc_aon::IomuxcAon", "IOMUXC_AON"),
        _ => todo!(),
    };

    let iomuxc_block_name = Ident::new(iomuxc_block_name, Span::call_site());
    let iomuxc_block = ir.blocks.get(iomuxc_block_path).unwrap();

    let mux_enum_base_name = format!("{reg}MuxMode");
    let mux_enum_pattern = format!("{instance}::vals::{mux_enum_base_name}");

    let (_, gpio_block) = struct_name.split_at(4);

    let (gpio_block, num) = if gpio_block.starts_with("Aon") {
        let (a, b) = gpio_block.split_at(3);
        (a, b.to_string())
    } else if gpio_block.starts_with("Emc") {
        let (block, num) = gpio_block.split_at(3);
        let (sub_block, num) = num.split_at(2);
        (block, format!("{sub_block}_{num}"))
    } else {
        let (a, b) = gpio_block.split_at(2);
        (a, b.to_string())
    };

    let gpio_name = format!("GPIO_{}_{}", gpio_block.to_uppercase(), num);
    let daisy_ctl_pattern = format!("SELECT_{gpio_name}_ALT");

    let mux_enum = ir.enums.get(&mux_enum_pattern).unwrap();

    let enums_for_pad: Vec<_> = ir
        .enums
        .iter()
        .filter_map(|(enum_name, enumm)| {
            let enum_variant = enumm
                .variants
                .iter()
                .find(|v| v.name.starts_with(&daisy_ctl_pattern))?;

            let (fs_name, _fs) = ir.fieldsets.iter().find(|(_, fs)| {
                let Some(field) = fs.fields.get(0) else {
                    return false;
                };

                field.enumm.as_ref() == Some(enum_name)
            })?;

            Some(Daisy {
                fs_name: fs_name.to_string(),
                enumm: enum_name.to_string(),
                variant: enum_variant.name.to_string(),
            })
        })
        .collect();

    let alt_modes: Vec<_> = mux_enum
        .variants
        .iter()
        .map(|variant| {
            let (alt, _) = variant.name.split_once("_").unwrap();
            let name = format!("SELECT_{gpio_name}_{alt}");

            let daisy = enums_for_pad
                .iter()
                .find(|daisy| daisy.variant == name)
                .cloned();

            AltMode {
                mux_enum: mux_enum_base_name.to_string(),
                mux_variant: variant.name.to_string(),
                daisy,
            }
        })
        .collect();

    let match_arms: Vec<_> = alt_modes
        .iter()
        .filter_map(|alt| alt.daisy.as_ref().map(|daisy| (alt, daisy)))
        .map(|(alt, daisy)| {
            let name = Ident::new(&alt.mux_enum, Span::call_site());
            let variant = Ident::new(&alt.mux_variant, Span::call_site());

            let enumm = daisy.enumm.split("::").last().unwrap();
            let iomuxc_block = ir.blocks.get("iomuxc::Iomuxc").unwrap();
            let iomuxc_aon_block = ir.blocks.get("iomuxc_aon::IomuxcAon").unwrap();

            let (daisy_block, daisy_fs) = iomuxc_block
                .items
                .iter()
                .map(|item| ("IOMUXC", item))
                .chain(
                    iomuxc_aon_block
                        .items
                        .iter()
                        .map(|item| ("IOMUXC_AON", item)),
                )
                .find_map(|(block, item)| match &item.inner {
                    BlockItemInner::Register(register) => {
                        if register.fieldset.as_ref().map(|v| v.as_str()) == Some(&daisy.fs_name) {
                            Some((block, item.name.as_str()))
                        } else {
                            None
                        }
                    }
                    BlockItemInner::Block(_) => None,
                })
                .expect(&daisy.fs_name);
            let daisy_block = Ident::new(daisy_block, Span::call_site());
            let daisy_fs = Ident::new(daisy_fs, Span::call_site());
            let daisy_name = Ident::new(enumm, Span::call_site());
            let daisy_variant = Ident::new(&daisy.variant, Span::call_site());

            quote! {
                #name::#variant => {
                    #daisy_block.
                        #daisy_fs().write(|w| w.set_daisy(#daisy_name::#daisy_variant));
                }
            }
        })
        .collect();

    let set_pac_alt_mode = {
        let iomuxc_mux_item_name = iomuxc_block
            .items
            .iter()
            .find_map(|item| match &item.inner {
                BlockItemInner::Register(register) => {
                    if register.fieldset.as_ref().map(|v| v.as_str()) == Some(fs_name) {
                        Some(item.name.clone())
                    } else {
                        None
                    }
                }
                BlockItemInner::Block(_) => None,
            })
            .unwrap();

        let item_name = Ident::new(&iomuxc_mux_item_name, Span::call_site());

        quote! {
            #iomuxc_block_name.#item_name().modify(|w| w.set_mux_mode(mux_mode));
        }
    };

    let alt_mode_body = if !match_arms.is_empty() {
        quote! {
            #set_pac_alt_mode
            match mux_mode {
                #(#match_arms)*
                _ => {},
            }
        }
    } else {
        quote! { #set_pac_alt_mode }
    };

    let gpio = Ident::new(struct_name, Span::call_site());
    let mux_enum = Ident::new(&mux_enum_base_name, Span::call_site());
    let output = quote::quote! {
        impl #gpio {
            fn set_alt_mode(mux_mode: #mux_enum) {
                #alt_mode_body
            }
        }
    };

    println!("{}", output);
}
