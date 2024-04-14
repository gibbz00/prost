use super::*;

impl CodeGenerator<'_> {
    pub(super) fn push_enums(&mut self, enum_types: Vec<EnumDescriptorProto>) {
        self.path.push(5);
        for (idx, desc) in enum_types.into_iter().enumerate() {
            self.path.push(idx as i32);
            if let Some(resolved_enum) = self.resolve_enum(desc) {
                self.buf.push_str(&resolved_enum.to_string());
            }
            self.path.pop();
        }
        self.path.pop();
    }

    pub(super) fn resolve_enums(
        &mut self,
        enum_type: Vec<EnumDescriptorProto>,
    ) -> Vec<TokenStream> {
        let mut enums = Vec::with_capacity(enum_type.len());

        self.path.push(4);
        for (idx, nested_enum) in enum_type.into_iter().enumerate() {
            self.path.push(idx as i32);
            if let Some(resolved_enum) = self.resolve_enum(nested_enum) {
                enums.push(resolved_enum);
            }
            self.path.pop();
        }
        self.path.pop();

        enums
    }

    fn resolve_enum(&mut self, desc: EnumDescriptorProto) -> Option<TokenStream> {
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
            return None;
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

        Some(quote! {
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
        })
    }

    pub(super) fn resolve_enum_attributes(
        &self,
        fq_message_name: &FullyQualifiedName,
    ) -> TokenStream {
        let type_attributes = self.config.type_attributes.get(fq_message_name.as_ref());
        let enum_attributes = self.config.enum_attributes.get(fq_message_name.as_ref());
        quote! {
            #(#(#type_attributes)*)*
            #(#(#enum_attributes)*)*
        }
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
}
