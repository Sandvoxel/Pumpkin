use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use std::{collections::BTreeMap, fs};

pub fn build() -> TokenStream {
    let java_json: BTreeMap<String, String> = serde_json::from_str(
        &fs::read_to_string("../../assets/en_us_java.json").expect("en_us_java is missing"),
    )
    .unwrap();

    let mut java_constants = TokenStream::new();
    let mut java_key_entries = TokenStream::new();
    for (name, value) in &java_json {
        let ident = to_valid_ident(name);
        let ident_str = ident.to_string();
        let doc = if !value.is_empty() {
            quote!(#[doc = #value])
        } else {
            quote!()
        };
        java_constants.extend(quote! {
            #doc
            pub const #ident: &str = #name;
        });
        java_key_entries.extend(quote! {
            #ident_str => #ident,
        });
    }

    let bedrock_content =
        fs::read_to_string("../../assets/en_us_bedrock.lang").expect("en_us_bedrock is missing");
    let mut bedrock_constants = TokenStream::new();
    let mut bedrock_key_entries = TokenStream::new();

    for line in bedrock_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('/') {
            continue;
        }

        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            let ident = to_valid_ident(name);
            let ident_str = ident.to_string();

            let doc = if !value.is_empty() {
                quote!(#[doc = #value])
            } else {
                quote!()
            };
            bedrock_constants.extend(quote! {
                #doc
                pub const #ident: &str = #name;
            });
            bedrock_key_entries.extend(quote! {
                #ident_str => #ident,
            });
        }
    }

    let mut java_value_entries = TokenStream::new();
    for (name, value) in &java_json {
        java_value_entries.extend(quote! {
            #name => #value,
        });
    }

    let mut bedrock_value_entries = TokenStream::new();
    for line in bedrock_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('/') {
            continue;
        }

        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            bedrock_value_entries.extend(quote! {
                #name => #value,
            });
        }
    }

    // --- Final Assembly ---
    quote! {
        #![allow(clippy::doc_markdown)]
        pub mod java {
            #java_constants
            static KEYS: phf::Map<&'static str, &'static str> = phf::phf_map! {
                #java_key_entries
            };
            static VALUES: phf::Map<&'static str, &'static str> = phf::phf_map! {
                #java_value_entries
            };
            pub fn get(const_name: &str) -> Option<&'static str> {
                KEYS.get(const_name).copied()
            }
            pub fn get_value(key: &str) -> Option<&'static str> {
                VALUES.get(key).copied()
            }
        }
        pub mod bedrock {
            #bedrock_constants
            static KEYS: phf::Map<&'static str, &'static str> = phf::phf_map! {
                #bedrock_key_entries
            };
            static VALUES: phf::Map<&'static str, &'static str> = phf::phf_map! {
                #bedrock_value_entries
            };
            pub fn get(const_name: &str) -> Option<&'static str> {
                KEYS.get(const_name).copied()
            }
            pub fn get_value(key: &str) -> Option<&'static str> {
                VALUES.get(key).copied()
            }
        }
    }
}

fn to_valid_ident(name: &str) -> Ident {
    let mut clean = name.to_uppercase().replace(['.', ':', '-'], "_");

    if clean.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        clean.insert(0, '_');
    }

    format_ident!("{}", clean)
}
