//! 工具实现的样板宏。
//!
//! 每个工具都是「一个 spec + 一个 async 处理函数」，用宏把
//! `Tool` trait 的转发代码收在一处，避免 40 多个工具写重复样板。

#[macro_export]
macro_rules! tool_impl {
    ($ty:ident, $name:literal, $description:literal, $parameters:expr, $handler:path) => {
        pub struct $ty;

        #[async_trait::async_trait]
        impl ::neosonia_core::registry::Tool for $ty {
            fn spec(&self) -> ::neosonia_core::model::ToolSpec {
                ::neosonia_core::model::ToolSpec::new($name, $description, $parameters)
            }

            async fn call(
                &self,
                ctx: &::neosonia_core::model::ToolCtx,
                args: ::serde_json::Value,
            ) -> ::neosonia_core::model::ToolOutcome {
                $handler(ctx, args).await
            }
        }
    };
    ($ty:ident, $name:literal, $description:literal, $parameters:expr, $handler:path, approval) => {
        pub struct $ty;

        #[async_trait::async_trait]
        impl ::neosonia_core::registry::Tool for $ty {
            fn spec(&self) -> ::neosonia_core::model::ToolSpec {
                ::neosonia_core::model::ToolSpec::new($name, $description, $parameters)
            }

            async fn call(
                &self,
                ctx: &::neosonia_core::model::ToolCtx,
                args: ::serde_json::Value,
            ) -> ::neosonia_core::model::ToolOutcome {
                $handler(ctx, args).await
            }

            fn requires_approval(&self, _args: &::serde_json::Value) -> bool {
                true
            }
        }
    };
}
