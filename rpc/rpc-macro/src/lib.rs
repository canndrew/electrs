/*
 * This is some half-written scratch-code for an attribute macro for generating JsonRpcService
 * implementations. The idea is that the macro will handle dispatching on method names, parsing
 * parameters and serializing the return value and will return the appropriate spec-compliant error
 * if anything fails.
 */

struct Signature {
    name: String,
    params: Vec<(Ident, Type)>,
}

impl Signature {
    fn parse_inputs(attr: Attribute, inputs: Vec<FnArg>) -> Signature {
        let name = match attr.parse_meta() {
            Meta::NameValue(meta_name_value) => {
                match meta_name_value.lit {
                    Lit::Str(lit_str) => lit_str.value(),
                }
            },
        };
        Signature {
            name,
        }
    }
}

struct MethodImpl {
    signature: Signature,
    return_type: Type,
}

struct JsonRpcImpl {
    methods: Vec<MethodImpl>,
    notifications: Vec<Signature>,
}

impl JsonRpcImpl {
    fn parse_impl(impl_item: ImplItem) -> JsonRpcImpl {
        let mut methods = Vec::new();
        for impl_item in impl_block.items {
            match impl_item {
                ImplItem::Method(impl_item_method) => {
                    for attr in impl_item_method.attrs {
                        if attr.path.is_ident("method") {
                            let signature = Signature::parse_inputs(attr, impl_item_method.signature.inputs);
                            let return_type = match impl_item_method.output {
                                ReturnType::Default => parse_quote! { () },
                                ReturnType::Type(_, return_type) => return_type,
                            };
                            let method = MethodImpl {
                                signature, return_type,
                            }
                            methods.push(method);
                        } else if attr.path.is_ident("notification") {
                            let signature = Signature::parse_inputs(attr, impl_item_method.signature.inputs);
                            notifications.push(signature);
                        } else {
                            todo!()
                        }
                    }
                },
            }
        }
        JsonRpcImpl { methods, notifications }
    }

    fn into_syn(self, self_type: Type) -> ImplItem {
        let method_branches = self.methods.into_iter().map(MethodImpl::into_syn()).collect();
        quote! {
            #[async_trait]
            impl JsonRpcService for #self_ty {
                async fn handle_method(&self, method: &str, params: Option<JsonRpcParams>)
                    -> Result<JsonValue, HandleMethodError>
                {
                    match method {
                        #(#method_branches),*
                        _ => Err(HandleMethodError::MethodNotFound),
                    }
                }
            }
        }
    }
}

#[proc_macro_attribute]
pub fn rpc_service(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut impl_block = syn::parse_macro_input!(item as syn::ItemImpl);
    // TODO: check other impl_block fields

    let self_ty = impl_block.self_ty;
    let json_rpc_impl = JsonRpcImpl::parse_impl(impl_block);
    quote! {
    }
}

