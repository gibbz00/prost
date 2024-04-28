use prost_types::{
    field_descriptor_proto::{Label, Type},
    FieldDescriptorProto,
};

use super::syntax::Syntax;

pub trait FieldDescriptorExt {
    fn optional(&self, syntax: Syntax) -> bool;
}

impl FieldDescriptorExt for FieldDescriptorProto {
    fn optional(&self, syntax: Syntax) -> bool {
        if self.proto3_optional.unwrap_or(false) {
            return true;
        }

        if self.label() != Label::Optional {
            return false;
        }

        match self.r#type() {
            Type::Message => true,
            _ => syntax == Syntax::Proto2,
        }
    }
}
