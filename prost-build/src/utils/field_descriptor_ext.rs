use prost_types::{
    field_descriptor_proto::{Label, Type},
    FieldDescriptorProto,
};

use crate::utils::*;

pub trait FieldDescriptorExt {
    fn repeated(&self) -> bool;

    fn optional(&self, syntax: Syntax) -> bool;
}

impl FieldDescriptorExt for FieldDescriptorProto {
    fn repeated(&self) -> bool {
        self.label == Some(Label::Repeated as i32)
    }

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
