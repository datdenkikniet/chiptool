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

    let block = |name: &str| {
        let block = ir.blocks.get(name).unwrap();
        block.items.iter().filter_map(|item| match &item.inner {
            BlockItemInner::Register(register) => Some((&item.name, &register.fieldset)),
            BlockItemInner::Block(_) => None,
        })
    };

    let iomuxc = block("iomuxc::Iomuxc");
    let iomuxc_aon = block("iomuxc_aon::IomuxcAon");

    let (iomux_for_alt, iomux_for_alt_name) = match instance.to_lowercase().as_str() {
        "iomuxc" => (iomuxc.clone(), "IOMUXC"),
        "iomuxc_aon" => (iomuxc_aon.clone(), "IOMUXC_AON"),
        _ => unimplemented!(),
    };

    let iomux_for_alt_name = Ident::new(iomux_for_alt_name, Span::call_site());

    let mux_enum_base_name = format!("{reg}MuxMode");
    let mux_enum_pattern = format!("{instance}::vals::{mux_enum_base_name}");

    let gpio_block_num = struct_name.strip_prefix("Gpio").unwrap();

    // Skip Dummy pins.
    if gpio_block_num.ends_with("Dummy") {
        return;
    }

    let (gpio_block, num) = if let Some(num) = gpio_block_num.strip_prefix("Aon") {
        ("Aon", num.to_string())
    } else if let Some(sub_block_num) = gpio_block_num.strip_prefix("Emc") {
        let (sub_block, num) = sub_block_num.split_at(2);
        ("Emc", format!("{sub_block}_{num}"))
    } else if let Some(sub_block_num) = gpio_block_num.strip_prefix("Sd") {
        let (sub_block, num) = sub_block_num.split_at(2);
        ("Sd", format!("{sub_block}_{num}"))
    } else {
        let (a, b) = gpio_block_num.split_at(2);
        (a, b.to_string())
    };

    let gpio_name = format!("GPIO_{}_{}", gpio_block.to_uppercase(), num);
    let daisy_ctl_prefix = &format!("SELECT_{gpio_name}_ALT"); // Don't include trailing number in prefix

    let mux_enum = ir.enums.get(&mux_enum_pattern).unwrap();

    // Daisy configurations for this pad
    let daisies_for_pad: Vec<_> = ir
        .enums
        .iter()
        // Assume that one daisy enum (= daisy mux) could route multiple
        // altmodes for the same pad (though this doesn't appear to happen
        // in practice)
        .flat_map(|(enum_name, enumm)| {
            enumm.variants.iter().filter_map(move |var| {
                var.name
                    .starts_with(daisy_ctl_prefix)
                    .then_some((enum_name, var))
            })
        })
        .filter_map(|(enum_name, enum_variant)| {
            let (fs_name, _) = ir
                .fieldsets
                .iter()
                .find(|(_, fs)| {
                    let Some(field) = fs.fields.get(0) else {
                        return false;
                    };

                    if field.enumm.as_ref() == Some(enum_name) {
                        assert_eq!(fs.fields.len(), 1, "Daisy must have 1 field");
                        true
                    } else {
                        false
                    }
                })
                .unwrap();

            Some(Daisy {
                fs_name: fs_name.to_string(),
                enumm: enum_name.to_string(),
                variant: enum_variant.name.to_string(),
            })
        })
        .collect();

    // The alt modes and their required daisy configuration (if any) for this pad
    let alt_modes: Vec<_> = mux_enum
        .variants
        .iter()
        .map(|variant| {
            let (alt, _) = variant.name.split_once("_").unwrap();
            let name = format!("SELECT_{gpio_name}_{alt}");

            let daisy = daisies_for_pad
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

            // Daisy configs are spread out over both blocks (wat?)
            let (daisy_block, daisy_fs) = iomuxc
                .clone()
                .map(|item| ("IOMUXC", item))
                .chain(iomuxc_aon.clone().map(|item| ("IOMUXC_AON", item)))
                .find_map(|(block, (item_name, fieldset))| {
                    if fieldset.as_ref().map(|v| v.as_str()) == Some(&daisy.fs_name) {
                        Some((block, item_name.as_str()))
                    } else {
                        None
                    }
                })
                .unwrap();
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

    if match_arms.is_empty() {
        eprintln!(
            "Found no input daisy for {}. That is probably wrong.",
            struct_name
        );
    }

    let set_alt_mode = {
        let iomuxc_mux_item_name = iomux_for_alt
            .clone()
            .find_map(|(item_name, fieldset)| {
                if fieldset.as_ref().map(|v| v.as_str()) == Some(fs_name) {
                    Some(item_name.clone())
                } else {
                    None
                }
            })
            .unwrap();

        let item_name = Ident::new(&iomuxc_mux_item_name, Span::call_site());

        quote! {
            #iomux_for_alt_name.#item_name().modify(|w| w.set_mux_mode(mux_mode));
        }
    };

    let set_input_daisy = (!match_arms.is_empty()).then(|| {
        quote! {
            match mux_mode {
                #(#match_arms)*
                _ => {},
            }
        }
    });

    let gpio = Ident::new(struct_name, Span::call_site());
    let mux_enum = Ident::new(&mux_enum_base_name, Span::call_site());
    let output = quote::quote! {
        impl #gpio {
            /// Configure alternate mode.
            #[allow(unused, reason = "only used for pins with peripheral impls")]
            #[inline(always)]
            pub(crate) fn set_alternate_mode(mux_mode: #mux_enum) {
                #set_alt_mode
                #set_input_daisy
            }
        }
    };

    println!("{}", output);
}
