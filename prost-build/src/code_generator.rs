use std::ascii;
use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;

use itertools::{Either, Itertools};
use log::debug;
use multimap::MultiMap;
use proc_macro2::TokenStream;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FieldOptions, FileDescriptorProto,
    OneofDescriptorProto, ServiceDescriptorProto, SourceCodeInfo,
};
use quote::{quote, ToTokens};
use syn::parse::Parser;
use syn::{Attribute, TypePath};

use crate::ast::{Comments, Method, Service};
use crate::extern_paths::ExternPaths;
use crate::ident::{strip_enum_prefix, to_snake, to_syn_ident, to_upper_camel};
use crate::message_graph::MessageGraph;
use crate::{Config, FullyQualifiedName};

mod c_escaping;
use c_escaping::unescape_c_escape_string;

#[derive(PartialEq)]
enum Syntax {
    Proto2,
    Proto3,
}

pub struct CodeGenerator<'a> {
    config: &'a mut Config,
    package: String,
    type_path: Vec<String>,
    source_info: Option<SourceCodeInfo>,
    syntax: Syntax,
    message_graph: &'a MessageGraph,
    extern_paths: &'a ExternPaths,
    path: Vec<i32>,
    buf: &'a mut String,
}

impl<'a> CodeGenerator<'a> {
    pub fn generate(
        config: &mut Config,
        message_graph: &MessageGraph,
        extern_paths: &ExternPaths,
        file: FileDescriptorProto,
        buf: &mut String,
    ) {
        let source_info = file.source_code_info.map(|mut s| {
            s.location.retain(|loc| {
                let len = loc.path.len();
                len > 0 && len % 2 == 0
            });
            s.location.sort_by(|a, b| a.path.cmp(&b.path));
            s
        });

        let syntax = match file.syntax.as_ref().map(String::as_str) {
            None | Some("proto2") => Syntax::Proto2,
            Some("proto3") => Syntax::Proto3,
            Some(s) => panic!("unknown syntax: {}", s),
        };

        let mut code_gen = CodeGenerator {
            config,
            package: file.package.unwrap_or_default(),
            type_path: Vec::new(),
            source_info,
            syntax,
            message_graph,
            extern_paths,
            path: Vec::new(),
            buf,
        };

        debug!(
            "file: {:?}, package: {:?}",
            file.name.as_ref().unwrap(),
            code_gen.package
        );

        code_gen.path.push(4);
        for (idx, message) in file.message_type.into_iter().enumerate() {
            code_gen.path.push(idx as i32);
            code_gen.append_message(message);
            code_gen.path.pop();
        }
        code_gen.path.pop();

        code_gen.path.push(5);
        for (idx, desc) in file.enum_type.into_iter().enumerate() {
            code_gen.path.push(idx as i32);
            code_gen.append_enum(desc);
            code_gen.path.pop();
        }
        code_gen.path.pop();

        if code_gen.config.service_generator.is_some() {
            code_gen.path.push(6);
            for (idx, service) in file.service.into_iter().enumerate() {
                code_gen.path.push(idx as i32);
                code_gen.push_service(service);
                code_gen.path.pop();
            }

            if let Some(service_generator) = code_gen.config.service_generator.as_mut() {
                service_generator.finalize(code_gen.buf);
            }

            code_gen.path.pop();
        }
    }

    fn append_message(&mut self, message: DescriptorProto) {
        debug!("  message: {:?}", message.name());

        let message_name = message.name().to_string();
        let fq_message_name =
            FullyQualifiedName::new(&self.package, &self.type_path, &message_name);

        // Skip external types.
        if self.extern_paths.resolve_ident(&fq_message_name).is_some() {
            return;
        }

        // Split the nested message types into a vector of normal nested message types, and a map
        // of the map field entry types. The path index of the nested message types is preserved so
        // that comments can be retrieved.
        type NestedTypes = Vec<(DescriptorProto, usize)>;
        type MapTypes = HashMap<String, (FieldDescriptorProto, FieldDescriptorProto)>;
        let (nested_types, map_types): (NestedTypes, MapTypes) = message
            .nested_type
            .into_iter()
            .enumerate()
            .partition_map(|(idx, nested_type)| {
                if nested_type
                    .options
                    .as_ref()
                    .and_then(|options| options.map_entry)
                    .unwrap_or(false)
                {
                    let key = nested_type.field[0].clone();
                    let value = nested_type.field[1].clone();
                    assert_eq!("key", key.name());
                    assert_eq!("value", value.name());
                    Either::Right((
                        fq_message_name
                            .join(nested_type.name())
                            .as_ref()
                            .to_string(),
                        (key, value),
                    ))
                } else {
                    Either::Left((nested_type, idx))
                }
            });

        // Split the fields into a vector of the normal fields, and oneof fields.
        // Path indexes are preserved so that comments can be retrieved.
        type Fields = Vec<(FieldDescriptorProto, usize)>;
        type OneofFields = MultiMap<i32, (FieldDescriptorProto, usize)>;
        let (fields, mut oneof_fields): (Fields, OneofFields) = message
            .field
            .into_iter()
            .enumerate()
            .partition_map(|(idx, field)| {
                if field.proto3_optional.unwrap_or(false) {
                    Either::Left((field, idx))
                } else if let Some(oneof_index) = field.oneof_index {
                    Either::Right((oneof_index, (field, idx)))
                } else {
                    Either::Left((field, idx))
                }
            });

        self.append_doc(&fq_message_name, None);
        self.append_message_attributes(&fq_message_name);

        let prost_path = self.prost_type_path("Message");
        let maybe_skip_debug = self.resolve_skip_debug(&fq_message_name);
        self.buf.push_str(&{
            quote! {
                #[allow(clippy::derive_partial_eq_without_eq)]
                #[derive(Clone, PartialEq, #prost_path)]
                #maybe_skip_debug
            }
            .to_string()
        });

        self.buf.push_str("pub struct ");
        self.buf.push_str(&to_upper_camel(&message_name));
        self.buf.push_str(" {\n");

        self.path.push(2);
        for (field, idx) in fields {
            self.path.push(idx as i32);
            match field
                .type_name
                .as_ref()
                .and_then(|type_name| map_types.get(type_name))
            {
                Some((key, value)) => self.append_map_field(&fq_message_name, field, key, value),
                None => self.append_field(&fq_message_name, field),
            }
            self.path.pop();
        }
        self.path.pop();

        self.path.push(8);
        for (idx, oneof) in message.oneof_decl.iter().enumerate() {
            let idx = idx as i32;

            let fields = match oneof_fields.get_vec(&idx) {
                Some(fields) => fields,
                None => continue,
            };

            self.path.push(idx);
            self.append_oneof_field(&message_name, &fq_message_name, oneof, fields);
            self.path.pop();
        }
        self.path.pop();

        self.buf.push_str("}\n");

        if !message.enum_type.is_empty() || !nested_types.is_empty() || !oneof_fields.is_empty() {
            self.buf.push_str("/// Nested message and enum types in `");
            self.buf.push_str(&message_name);
            self.buf.push_str("`.\n");
            self.buf.push_str("pub mod ");
            self.buf.push_str(&to_snake(&message_name));
            self.buf.push_str(" {\n");
            self.type_path.push(message_name.clone());

            self.path.push(3);
            for (nested_type, idx) in nested_types {
                self.path.push(idx as i32);
                self.append_message(nested_type);
                self.path.pop();
            }
            self.path.pop();

            self.path.push(4);
            for (idx, nested_enum) in message.enum_type.into_iter().enumerate() {
                self.path.push(idx as i32);
                self.append_enum(nested_enum);
                self.path.pop();
            }
            self.path.pop();

            for (idx, oneof) in message.oneof_decl.into_iter().enumerate() {
                let idx = idx as i32;
                // optional fields create a synthetic oneof that we want to skip
                let fields = match oneof_fields.remove(&idx) {
                    Some(fields) => fields,
                    None => continue,
                };
                self.append_oneof(&fq_message_name, oneof, idx, fields);
            }

            self.type_path.pop();
            self.buf.push_str("}\n"); // end of module
        }

        if self.config.enable_type_names {
            self.append_type_name(&message_name, &fq_message_name);
        }
    }

    fn append_type_name(&mut self, message_name: &str, fq_message_name: &FullyQualifiedName) {
        self.buf.push_str(&{
            let name_path = self.prost_type_path("Name");
            let message_name_syn = to_syn_ident(message_name);
            let package_name = &self.package;
            let string_path = self.prost_type_path("alloc::string::String");
            let fully_qualified_name =
                FullyQualifiedName::new(&self.package, &self.type_path, message_name);
            let domain_name = self
                .config
                .type_name_domains
                .get_first(fq_message_name.as_ref())
                .map_or("", |name| name.as_str());

            let fq_name_str = fully_qualified_name.as_ref().trim_start_matches('.');
            let type_url = format!("{}/{}", domain_name, fq_name_str);

            quote! {
                impl #name_path for #message_name_syn {
                    const NAME: &'static str = #message_name;
                    const PACKAGE: &'static str = #package_name;

                    fn full_name() -> #string_path { #fq_name_str.into() }
                    fn type_url() -> #string_path { #type_url.into() }
                }
            }
            .to_string()
        });
    }

    fn append_message_attributes(&mut self, fq_message_name: &FullyQualifiedName) {
        self.buf.push_str(&{
            let type_attributes = self.config.type_attributes.get(fq_message_name.as_ref());
            let message_attributes = self.config.message_attributes.get(fq_message_name.as_ref());
            quote! {
                #(#(#type_attributes)*)*
                #(#(#message_attributes)*)*
            }
            .to_string()
        });
    }

    fn resolve_enum_attributes(&self, fq_message_name: &FullyQualifiedName) -> TokenStream {
        let type_attributes = self.config.type_attributes.get(fq_message_name.as_ref());
        let enum_attributes = self.config.enum_attributes.get(fq_message_name.as_ref());
        quote! {
            #(#(#type_attributes)*)*
            #(#(#enum_attributes)*)*
        }
    }

    fn should_skip_debug(&self, fq_message_name: &FullyQualifiedName) -> bool {
        self.config
            .skip_debug
            .get(fq_message_name.as_ref())
            .next()
            .is_some()
    }

    fn resolve_skip_debug(&mut self, fq_message_name: &FullyQualifiedName) -> Option<TokenStream> {
        self.should_skip_debug(fq_message_name)
            .then_some(quote! { #[prost(skip_debug)] })
    }

    // TEMP: deprecate
    fn append_field_attributes(
        &mut self,
        fully_qualified_name: &FullyQualifiedName,
        field_name: &str,
    ) {
        self.buf.push_str(
            &self
                .resolve_field_attributes(fully_qualified_name, field_name)
                .to_string(),
        )
    }

    fn resolve_field_attributes(
        &self,
        fully_qualified_name: &FullyQualifiedName,
        field_name: &str,
    ) -> TokenStream {
        let fq_str = fully_qualified_name.as_ref();
        let field_attributes = self.config.field_attributes.get_field(fq_str, field_name);

        quote! {
            #(#(#field_attributes)*)*
        }
    }

    fn append_field(&mut self, fq_message_name: &FullyQualifiedName, field: FieldDescriptorProto) {
        let type_ = field.r#type();
        let repeated = field.label == Some(Label::Repeated as i32);
        let optional = self.optional(&field);
        let ty = self.resolve_type(&field, fq_message_name);

        let boxed = !repeated
            && ((type_ == Type::Message || type_ == Type::Group)
                && self
                    .message_graph
                    .is_nested(field.type_name(), fq_message_name.as_ref()))
            || (self
                .config
                .boxed
                .get_first_field(fq_message_name, field.name())
                .is_some());

        debug!(
            "    field: {:?}, type: {:?}, boxed: {}",
            field.name(),
            ty,
            boxed
        );

        self.append_doc(fq_message_name, Some(field.name()));

        if field
            .options
            .as_ref()
            .map(FieldOptions::deprecated)
            .unwrap_or_default()
        {
            self.buf.push_str("#[deprecated]\n");
        }

        self.buf.push_str("#[prost(");
        self.buf.push_str(&self.field_type_tag(&field));

        if type_ == Type::Bytes {
            let bytes_type = self
                .config
                .bytes_type
                .get_first_field(fq_message_name, field.name())
                .copied()
                .unwrap_or_default();
            self.buf
                .push_str(&format!("={:?}", bytes_type.annotation()));
        }

        match field.label() {
            Label::Optional => {
                if optional {
                    self.buf.push_str(", optional");
                }
            }
            Label::Required => self.buf.push_str(", required"),
            Label::Repeated => {
                self.buf.push_str(", repeated");
                if can_pack(&field)
                    && !field
                        .options
                        .as_ref()
                        .map_or(self.syntax == Syntax::Proto3, |options| options.packed())
                {
                    self.buf.push_str(", packed=\"false\"");
                }
            }
        }

        if boxed {
            self.buf.push_str(", boxed");
        }
        self.buf.push_str(", tag=\"");
        self.buf.push_str(&field.number().to_string());

        if let Some(ref default) = field.default_value {
            self.buf.push_str("\", default=\"");
            match type_ {
                Type::Bytes => {
                    self.buf.push_str("b\\\"");
                    for b in unescape_c_escape_string(default) {
                        self.buf.extend(
                            ascii::escape_default(b).flat_map(|c| (c as char).escape_default()),
                        );
                    }
                    self.buf.push_str("\\\"");
                }
                Type::Enum => {
                    let mut enum_value = to_upper_camel(default);
                    if self.config.strip_enum_prefix {
                        let enum_type = field
                            .type_name
                            .as_ref()
                            .and_then(|ty| ty.split('.').last())
                            .expect("field type not fully qualified");

                        enum_value = strip_enum_prefix(&to_upper_camel(enum_type), &enum_value)
                    }
                    self.buf.push_str(&enum_value);
                }
                _ => {
                    self.buf.push_str(&default.escape_default().to_string());
                }
            }
        }

        self.buf.push_str("\")]\n");
        self.append_field_attributes(fq_message_name, field.name());
        self.buf.push_str("pub ");
        self.buf.push_str(&to_snake(field.name()));
        self.buf.push_str(": ");

        if repeated {
            self.buf
                .push_str(&format!("{}::alloc::vec::Vec<", self.resolve_prost_path()));
        } else if optional {
            self.buf.push_str("::core::option::Option<");
        }
        if boxed {
            self.buf.push_str(&format!(
                "{}::alloc::boxed::Box<",
                self.resolve_prost_path()
            ));
        }
        self.buf.push_str(&ty);
        if boxed {
            self.buf.push('>');
        }
        if repeated || optional {
            self.buf.push('>');
        }
        self.buf.push_str(",\n");
    }

    fn append_map_field(
        &mut self,
        fq_message_name: &FullyQualifiedName,
        field: FieldDescriptorProto,
        key: &FieldDescriptorProto,
        value: &FieldDescriptorProto,
    ) {
        let key_ty = self.resolve_type(key, fq_message_name);
        let value_ty = self.resolve_type(value, fq_message_name);

        debug!(
            "    map field: {:?}, key type: {:?}, value type: {:?}",
            field.name(),
            key_ty,
            value_ty
        );

        self.append_doc(fq_message_name, Some(field.name()));

        let map_type = self
            .config
            .map_type
            .get_first_field(fq_message_name, field.name())
            .copied()
            .unwrap_or_default();
        let key_tag = self.field_type_tag(key);
        let value_tag = self.map_value_type_tag(value);

        self.buf.push_str(&format!(
            "#[prost({}=\"{}, {}\", tag=\"{}\")]\n",
            map_type.annotation(),
            key_tag,
            value_tag,
            field.number()
        ));
        self.append_field_attributes(fq_message_name, field.name());
        self.buf.push_str(&format!(
            "pub {}: {}<{}, {}>,\n",
            to_snake(field.name()),
            map_type.rust_type(),
            key_ty,
            value_ty
        ));
    }

    fn append_oneof_field(
        &mut self,
        message_name: &str,
        fq_message_name: &FullyQualifiedName,
        oneof: &OneofDescriptorProto,
        fields: &[(FieldDescriptorProto, usize)],
    ) {
        let name = format!(
            "{}::{}",
            to_snake(message_name),
            to_upper_camel(oneof.name())
        );
        self.append_doc(fq_message_name, None);
        self.buf.push_str(&format!(
            "#[prost(oneof=\"{}\", tags=\"{}\")]\n",
            name,
            fields.iter().map(|(field, _)| field.number()).join(", ")
        ));
        self.append_field_attributes(fq_message_name, oneof.name());
        self.buf.push_str(&format!(
            "pub {}: ::core::option::Option<{}>,\n",
            to_snake(oneof.name()),
            name
        ));
    }

    fn append_oneof(
        &mut self,
        fq_message_name: &FullyQualifiedName,
        oneof: OneofDescriptorProto,
        idx: i32,
        fields: Vec<(FieldDescriptorProto, usize)>,
    ) {
        self.path.push(8);
        self.path.push(idx);
        let documentation = self.resolve_docs(fq_message_name, None);
        self.path.pop();
        self.path.pop();

        let oneof_name = fq_message_name.join(oneof.name());
        let enum_attributes = self.resolve_enum_attributes(&oneof_name);
        let maybe_skip_debug = self.resolve_skip_debug(fq_message_name);
        let enum_name = to_syn_ident(&to_upper_camel(oneof.name()));
        let variants = self.oneof_variants(&fields, fq_message_name, &oneof_name);

        self.buf.push_str(&{
            let one_of_path = self.prost_type_path("Oneof");
            quote! {
                #(#documentation)*
                #enum_attributes
                #[allow(clippy::derive_partial_eq_without_eq)]
                #[derive(Clone, PartialEq, #one_of_path)]
                #maybe_skip_debug
                pub enum #enum_name {
                    #(#variants,)*
                }
            }
            .to_string()
        });
    }

    fn oneof_variants(
        &mut self,
        fields: &[(FieldDescriptorProto, usize)],
        fq_message_name: &FullyQualifiedName,
        oneof_name: &FullyQualifiedName,
    ) -> Vec<TokenStream> {
        let mut variants = Vec::with_capacity(fields.len());

        self.path.push(2);
        for (field, idx) in fields {
            self.path.push((*idx).try_into().expect("idx overflow"));
            let documentation = self.resolve_docs(fq_message_name, Some(field.name()));
            self.path.pop();

            let ty_tag = syn::parse_str::<syn::Meta>(&self.field_type_tag(field))
                .expect("unable to parse meta");
            let field_attributes = self.resolve_field_attributes(oneof_name, field.name());

            let rust_type = self.resolve_type(field, fq_message_name);

            let boxed = (matches!(field.r#type(), Type::Message | Type::Group)
                && self
                    .message_graph
                    .is_nested(field.type_name(), fq_message_name.as_ref()))
                || (self
                    .config
                    .boxed
                    .get_first_field(oneof_name, field.name())
                    .is_some());

            debug!(
                "    oneof: {}, type: {}, boxed: {}",
                field.name(),
                rust_type,
                boxed
            );

            let field_name = to_syn_ident(&to_upper_camel(field.name()));
            let type_path =
                syn::parse_str::<syn::Path>(&rust_type).expect("unable to parse type path");
            let enum_variant = match boxed {
                true => quote! {
                    #field_name(::prost::alloc::boxed::Box<#type_path>)
                },
                false => quote! {
                    #field_name(#type_path)
                },
            };

            let field_number_string = field.number().to_string();
            variants.push(quote! {
                 #(#documentation)*
                 #[prost(#ty_tag, tag=#field_number_string)]
                 #field_attributes
                 #enum_variant
            });
        }
        self.path.pop();

        variants
    }

    fn comments_from_location(&self) -> Option<Comments> {
        let source_info = self.source_info.as_ref()?;
        let idx = source_info
            .location
            .binary_search_by_key(&&self.path[..], |location| &location.path[..])
            .unwrap();
        Some(Comments::from_location(&source_info.location[idx]))
    }

    // TEMP: deprecate
    fn append_doc(&mut self, fq_name: &FullyQualifiedName, field_name: Option<&str>) {
        for doc in self.resolve_docs(fq_name, field_name) {
            self.buf.push_str(&doc.to_token_stream().to_string());
        }
    }

    fn resolve_docs(
        &self,
        fq_name: &FullyQualifiedName,
        field_name: Option<&str>,
    ) -> Vec<Attribute> {
        let mut comment_string = String::new();
        let disable_comments = &self.config.disable_comments;
        let append_doc = match field_name {
            Some(field_name) => disable_comments.get_first_field(fq_name, field_name),
            None => disable_comments.get(fq_name.as_ref()).next(),
        }
        .is_none();

        if append_doc {
            if let Some(comments) = self.comments_from_location() {
                comments.append_with_indent(&mut comment_string);
            }
        }

        match comment_string.is_empty() {
            true => Vec::new(),
            false => Attribute::parse_outer
                .parse_str(&comment_string)
                .expect("unable to parse comment attribute"),
        }
    }

    fn append_enum(&mut self, desc: EnumDescriptorProto) {
        debug!("  enum: {:?}", desc.name());

        let proto_enum_name = desc.name();
        let enum_name = to_upper_camel(proto_enum_name);
        let fq_proto_enum_name =
            FullyQualifiedName::new(&self.package, &self.type_path, proto_enum_name);

        if self
            .extern_paths
            .resolve_ident(&fq_proto_enum_name)
            .is_some()
        {
            return;
        }

        let enum_docs = self.resolve_docs(&fq_proto_enum_name, None);
        let enum_attributes = self.resolve_enum_attributes(&fq_proto_enum_name);
        let prost_path = self.prost_type_path("Enumeration");
        let optional_debug =
            (!self.should_skip_debug(&fq_proto_enum_name)).then_some(quote! {#[derive(Debug)]});
        let variant_mappings = EnumVariantMapping::build_enum_value_mappings(
            &enum_name,
            self.config.strip_enum_prefix,
            &desc.value,
        );
        let enum_variants = self.resolve_enum_variants(&variant_mappings, &fq_proto_enum_name);
        let enum_name_syn = to_syn_ident(&enum_name);
        let arms_1 = variant_mappings.iter().map(|variant| {
            syn::parse_str::<syn::Arm>(&format!(
                "{}::{} => \"{}\"",
                enum_name_syn, variant.generated_variant_name, variant.proto_name
            ))
            .expect("unable to parse enum arm")
            .to_token_stream()
        });
        let arms_2 = variant_mappings.iter().map(|variant| {
            syn::parse_str::<syn::Arm>(&format!(
                "\"{}\" => Some(Self::{})",
                variant.proto_name, variant.generated_variant_name
            ))
            .expect("unable to parse enum arm")
            .to_token_stream()
        });

        self.buf.push_str(&{
            quote! {
                #(#enum_docs)*
                #enum_attributes
                #optional_debug
                #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, #prost_path)]
                #[repr(i32)]
                pub enum #enum_name_syn {
                    #(#enum_variants,)*
                }

                impl #enum_name_syn {
                    /// String value of the enum field names used in the ProtoBuf definition.
                    ///
                    /// The values are not transformed in any way and thus are considered stable
                    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
                    pub fn as_str_name(&self) -> &'static str {
                        match self {
                            #(#arms_1,)*
                        }
                    }

                    /// Creates an enum from field names used in the ProtoBuf definition.
                    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
                        match value {
                            #(#arms_2,)*
                            _ => None,
                        }
                    }
                }
            }
            .to_string()
        });
    }

    fn resolve_enum_variants(
        &mut self,
        variant_mappings: &[EnumVariantMapping],
        fq_proto_enum_name: &FullyQualifiedName,
    ) -> Vec<TokenStream> {
        let mut variants = Vec::with_capacity(variant_mappings.len());

        self.path.push(2);

        for variant in variant_mappings.iter() {
            self.path.push(variant.path_idx as i32);

            let documentation = self.resolve_docs(fq_proto_enum_name, Some(variant.proto_name));

            let field_attributes =
                self.resolve_field_attributes(fq_proto_enum_name, variant.proto_name);

            let variant = syn::parse_str::<syn::Variant>(&format!(
                "{} = {}",
                variant.generated_variant_name, variant.proto_number
            ))
            .expect("unable to parse enum variant");

            variants.push(quote! {
                #(#documentation)*
                #field_attributes
                #variant
            });

            self.path.pop();
        }

        self.path.pop();

        variants
    }

    fn push_service(&mut self, service: ServiceDescriptorProto) {
        let name = service.name().to_owned();
        debug!("  service: {:?}", name);

        let comments = self.comments_from_location().unwrap_or_default();

        self.path.push(2);
        let methods = service
            .method
            .into_iter()
            .enumerate()
            .map(|(idx, mut method)| {
                debug!("  method: {:?}", method.name());

                self.path.push(idx as i32);
                let comments = self.comments_from_location().unwrap_or_default();
                self.path.pop();

                let name = method.name.take().unwrap();
                let input_proto_type = method.input_type.take().unwrap();
                let output_proto_type = method.output_type.take().unwrap();
                let input_type =
                    self.resolve_ident(&FullyQualifiedName::from_type_name(&input_proto_type));
                let output_type =
                    self.resolve_ident(&FullyQualifiedName::from_type_name(&output_proto_type));
                let client_streaming = method.client_streaming();
                let server_streaming = method.server_streaming();

                Method {
                    name: to_snake(&name),
                    proto_name: name,
                    comments,
                    input_type,
                    output_type,
                    input_proto_type,
                    output_proto_type,
                    options: method.options.unwrap_or_default(),
                    client_streaming,
                    server_streaming,
                }
            })
            .collect();
        self.path.pop();

        let service = Service {
            name: to_upper_camel(&name),
            proto_name: name,
            package: self.package.clone(),
            comments,
            methods,
            options: service.options.unwrap_or_default(),
        };

        if let Some(service_generator) = self.config.service_generator.as_mut() {
            service_generator.generate(service, self.buf)
        }
    }

    fn resolve_type(
        &self,
        field: &FieldDescriptorProto,
        fq_message_name: &FullyQualifiedName,
    ) -> String {
        match field.r#type() {
            Type::Float => String::from("f32"),
            Type::Double => String::from("f64"),
            Type::Uint32 | Type::Fixed32 => String::from("u32"),
            Type::Uint64 | Type::Fixed64 => String::from("u64"),
            Type::Int32 | Type::Sfixed32 | Type::Sint32 | Type::Enum => String::from("i32"),
            Type::Int64 | Type::Sfixed64 | Type::Sint64 => String::from("i64"),
            Type::Bool => String::from("bool"),
            Type::String => format!("{}::alloc::string::String", self.resolve_prost_path()),
            Type::Bytes => self
                .config
                .bytes_type
                .get_first_field(fq_message_name, field.name())
                .copied()
                .unwrap_or_default()
                .rust_type()
                .to_owned(),
            Type::Group | Type::Message => {
                self.resolve_ident(&FullyQualifiedName::from_type_name(field.type_name()))
            }
        }
    }

    fn resolve_ident(&self, pb_ident: &FullyQualifiedName) -> String {
        if let Some(proto_ident) = self.extern_paths.resolve_ident(pb_ident) {
            return proto_ident;
        }

        let mut local_path = self
            .package
            .split('.')
            .chain(self.type_path.iter().map(String::as_str))
            .peekable();

        // If no package is specified the start of the package name will be '.'
        // and split will return an empty string ("") which breaks resolution
        // The fix to this is to ignore the first item if it is empty.
        if local_path.peek().map_or(false, |s| s.is_empty()) {
            local_path.next();
        }

        let mut ident_path = pb_ident.path_iterator();
        let ident_type = ident_path.next_back().unwrap();
        let mut ident_path = ident_path.peekable();

        // Skip path elements in common.
        while local_path.peek().is_some() && local_path.peek() == ident_path.peek() {
            local_path.next();
            ident_path.next();
        }

        local_path
            .map(|_| "super".to_string())
            .chain(ident_path.map(to_snake))
            .chain(iter::once(to_upper_camel(ident_type)))
            .join("::")
    }

    fn field_type_tag(&self, field: &FieldDescriptorProto) -> Cow<'static, str> {
        match field.r#type() {
            Type::Float => Cow::Borrowed("float"),
            Type::Double => Cow::Borrowed("double"),
            Type::Int32 => Cow::Borrowed("int32"),
            Type::Int64 => Cow::Borrowed("int64"),
            Type::Uint32 => Cow::Borrowed("uint32"),
            Type::Uint64 => Cow::Borrowed("uint64"),
            Type::Sint32 => Cow::Borrowed("sint32"),
            Type::Sint64 => Cow::Borrowed("sint64"),
            Type::Fixed32 => Cow::Borrowed("fixed32"),
            Type::Fixed64 => Cow::Borrowed("fixed64"),
            Type::Sfixed32 => Cow::Borrowed("sfixed32"),
            Type::Sfixed64 => Cow::Borrowed("sfixed64"),
            Type::Bool => Cow::Borrowed("bool"),
            Type::String => Cow::Borrowed("string"),
            Type::Bytes => Cow::Borrowed("bytes"),
            Type::Group => Cow::Borrowed("group"),
            Type::Message => Cow::Borrowed("message"),
            Type::Enum => Cow::Owned(format!(
                "enumeration={:?}",
                self.resolve_ident(&FullyQualifiedName::from_type_name(field.type_name()))
            )),
        }
    }

    fn map_value_type_tag(&self, field: &FieldDescriptorProto) -> Cow<'static, str> {
        match field.r#type() {
            Type::Enum => Cow::Owned(format!(
                "enumeration({})",
                self.resolve_ident(&FullyQualifiedName::from_type_name(field.type_name()))
            )),
            _ => self.field_type_tag(field),
        }
    }

    fn optional(&self, field: &FieldDescriptorProto) -> bool {
        if field.proto3_optional.unwrap_or(false) {
            return true;
        }

        if field.label() != Label::Optional {
            return false;
        }

        match field.r#type() {
            Type::Message => true,
            _ => self.syntax == Syntax::Proto2,
        }
    }

    fn resolve_prost_path(&self) -> &str {
        self.config.prost_path.as_deref().unwrap_or("::prost")
    }

    fn prost_type_path(&self, item: &str) -> TypePath {
        syn::parse_str(&format!("{}::{}", self.resolve_prost_path(), item))
            .expect("unable to parse prost type path")
    }
}

/// Returns `true` if the repeated field type can be packed.
fn can_pack(field: &FieldDescriptorProto) -> bool {
    matches!(
        field.r#type(),
        Type::Float
            | Type::Double
            | Type::Int32
            | Type::Int64
            | Type::Uint32
            | Type::Uint64
            | Type::Sint32
            | Type::Sint64
            | Type::Fixed32
            | Type::Fixed64
            | Type::Sfixed32
            | Type::Sfixed64
            | Type::Bool
            | Type::Enum
    )
}

use enum_variant_mapping::EnumVariantMapping;
mod enum_variant_mapping {
    use std::collections::{HashMap, HashSet};

    use prost_types::EnumValueDescriptorProto;

    use crate::ident::to_upper_camel;

    use super::strip_enum_prefix;

    pub(super) struct EnumVariantMapping<'a> {
        pub(super) path_idx: usize,
        pub(super) proto_name: &'a str,
        pub(super) proto_number: i32,
        pub(super) generated_variant_name: String,
    }

    impl EnumVariantMapping<'_> {
        pub(super) fn build_enum_value_mappings<'a>(
            generated_enum_name: &str,
            do_strip_enum_prefix: bool,
            enum_values: &'a [EnumValueDescriptorProto],
        ) -> Vec<EnumVariantMapping<'a>> {
            let mut numbers = HashSet::new();
            let mut generated_names = HashMap::new();
            let mut mappings = Vec::new();

            for (idx, value) in enum_values.iter().enumerate() {
                // Skip duplicate enum values. Protobuf allows this when the
                // 'allow_alias' option is set.
                if !numbers.insert(value.number()) {
                    continue;
                }

                let mut generated_variant_name = to_upper_camel(value.name());
                if do_strip_enum_prefix {
                    generated_variant_name =
                        strip_enum_prefix(generated_enum_name, &generated_variant_name);
                }

                if let Some(old_v) =
                    generated_names.insert(generated_variant_name.to_owned(), value.name())
                {
                    panic!("Generated enum variant names overlap: `{}` variant name to be used both by `{}` and `{}` ProtoBuf enum values",
                    generated_variant_name, old_v, value.name());
                }

                mappings.push(EnumVariantMapping {
                    path_idx: idx,
                    proto_name: value.name(),
                    proto_number: value.number(),
                    generated_variant_name,
                })
            }
            mappings
        }
    }
}
