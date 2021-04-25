extern crate proc_macro;
use {
    std::collections::HashMap,
    syn::{
        Ident, Type, Meta, FnArg, Lit, Pat, parse_quote, ItemImpl, ImplItem, Receiver,
        ReturnType, Generics, Token, braced, Visibility, TraitItem, parse, Abi,
        GenericParam,
        token::Unsafe,
        spanned::Spanned,
        parse::{Parse, ParseStream},
    },
    quote::{quote, quote_spanned},
    proc_macro2::{Span, TokenStream},
    proc_macro_error::{proc_macro_error, abort},
    crate::{
        service::JsonRpcServiceImpl,
        client::{JsonRpcClientImpl, JsonRpcClientImplSyntax},
    },
};
mod service;
mod client;

#[proc_macro_error]
#[proc_macro_attribute]
pub fn json_rpc_service(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let mut impl_block = syn::parse_macro_input!(item as syn::ItemImpl);
    let json_rpc_impl = JsonRpcServiceImpl::parse_impl(&mut impl_block);
    let rpc_impl = json_rpc_impl.into_syn();
    let tokens = quote! {
        #impl_block
        #rpc_impl
    };
    tokens.into()
}

#[proc_macro_error]
#[proc_macro]
pub fn json_rpc_client(
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let client_syntax = syn::parse_macro_input!(item as JsonRpcClientImplSyntax);
    let client_type = JsonRpcClientImpl::parse_client_syntax(client_syntax);
    let tokens = client_type.into_syn();
    tokens.into()
}

