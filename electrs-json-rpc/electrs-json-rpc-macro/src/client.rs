use super::*;

pub struct JsonRpcClientImplSyntax {
    visibility: Visibility,
    #[allow(unused)] // FIXME
    type_token: Token![type],
    ident: Ident,
    #[allow(unused)]
    brace_token: token::Brace,
    items: Vec<TraitItem>,
    full_span: Span,
}

impl Parse for JsonRpcClientImplSyntax {
    fn parse(input: ParseStream) -> Result<JsonRpcClientImplSyntax, parse::Error> {
        let visibility = input.parse()?;
        let type_token = input.parse()?;
        let ident = input.parse()?;
        let content;
        let brace_token = braced!(content in input);
        let mut items = Vec::new();
        while !content.is_empty() {
            items.push(content.parse()?);
        }
        let full_span = input.span();
        Ok(JsonRpcClientImplSyntax {
            visibility, type_token, ident, brace_token, items, full_span,
        })
    }
}

pub struct JsonRpcClientImpl {
    visibility: Visibility,
    name: Ident,
    notifications: HashMap<Ident, ClientMethodSignature>,
    methods: HashMap<Ident, ClientMethodSignature>,
    full_span: Span,
}

impl JsonRpcClientImpl {
    pub fn parse_client_syntax(client_syntax: JsonRpcClientImplSyntax) -> JsonRpcClientImpl {
        let visibility = client_syntax.visibility;
        let name = client_syntax.ident;
        let mut notifications = HashMap::new();
        let mut methods = HashMap::new();
        for trait_item in client_syntax.items {
            let mut trait_item_method = match trait_item {
                TraitItem::Method(trait_item_method) => trait_item_method,
                _ => {
                    abort!(
                        trait_item.span(),
                        "only methods are allowed in json_rpc_client! blocks",
                    )
                },
            };
            let attr = {
                let index_opt = trait_item_method.attrs.iter().position(|attr| {
                    attr.path.is_ident("method") || attr.path.is_ident("notification")
                });
                match index_opt {
                    Some(index) => trait_item_method.attrs.remove(index),
                    None => {
                        abort!(
                            trait_item_method.span(),
                            "missing #[notification] attribute",
                        )
                    },
                }
            };
            if attr.path.is_ident("method") {
                let name = match attr.parse_meta().unwrap() {
                    Meta::NameValue(meta_name_value) => {
                        match meta_name_value.lit {
                            Lit::Str(lit_str) => lit_str.value(),
                            _ => {
                                abort!(
                                    meta_name_value.lit.span(),
                                    "notification name must be a string",
                                )
                            },
                        }
                    },
                    _ => {
                        abort!(
                            attr.span(),
                            "Invalid use of attribute";
                            help = "Syntax is #[notification = \"method_name\"]",
                        )
                    },
                };
                let method_name = trait_item_method.sig.ident.clone();
                let signature = {
                    ClientMethodSignature::parse_signature(
                        &trait_item_method.sig,
                        name.clone(),
                    )
                };
                match methods.insert(method_name, signature) {
                    Some(prev_signature) => {
                        abort!(
                            trait_item_method.span(),
                            "multiple methods named {}", name;
                            note = prev_signature.sig_span => "previous definition here",
                        );
                    },
                    None => (),
                }
            }
            if attr.path.is_ident("notification") {
                let name = match attr.parse_meta().unwrap() {
                    Meta::NameValue(meta_name_value) => {
                        match meta_name_value.lit {
                            Lit::Str(lit_str) => lit_str.value(),
                            _ => {
                                abort!(
                                    meta_name_value.lit.span(),
                                    "notification name must be a string",
                                )
                            },
                        }
                    },
                    _ => {
                        abort!(
                            attr.span(),
                            "Invalid use of attribute";
                            help = "Syntax is #[notification = \"method_name\"]",
                        )
                    },
                };
                let method_name = trait_item_method.sig.ident.clone();
                let signature = {
                    ClientMethodSignature::parse_signature(
                        &trait_item_method.sig,
                        name.clone(),
                    )
                };
                match notifications.insert(method_name, signature) {
                    Some(prev_signature) => {
                        abort!(
                            trait_item_method.span(),
                            "multiple methods named {}", name;
                            note = prev_signature.sig_span => "previous definition here",
                        );
                    },
                    None => (),
                }
            }
        }
        let full_span = client_syntax.full_span;
        JsonRpcClientImpl { visibility, name, notifications, methods, full_span }
    }

    pub fn into_syn(&self) -> TokenStream {
        let visibility = &self.visibility;
        let name = &self.name;
        let mut notification_impls = Vec::with_capacity(self.notifications.len());
        let mut method_impls = Vec::with_capacity(self.methods.len());
        for (impl_name, signature) in &self.notifications {
            let name = &signature.name;
            let params_len = signature.params.len();
            let param_sigs = signature.param_sigs();
            let params_to_json = signature.params_to_json(parse_quote! { ClientSendNotificationError });
            let receiver = &signature.receiver;
            let return_type = &signature.return_type;
            let sig_span = signature.sig_span;
            let notification = quote_spanned! {sig_span=>
                async fn #impl_name(#receiver, #(#param_sigs,)*) -> #return_type {
                    let __params = if #params_len > 0 {
                        let mut __params_array = Vec::with_capacity(#params_len);
                        #(#params_to_json)*
                        Some(JsonRpcParams::Array(__params_array))
                    } else {
                        None
                    };
                    match self.client.notify(#name, __params).await {
                        Ok(()) => Ok(()),
                        Err(ClientSendRequestError::Io { source }) => {
                            Err(ClientSendNotificationError::Io { source })
                        },
                        Err(SendMesageError::ConnectionDropped) => {
                            Err(ClientSendNotificationError::ConnectionDropped)
                        },
                    }
                }
            };
            notification_impls.push(notification);
        }
        for (impl_name, signature) in &self.methods {
            let name = &signature.name;
            let params_len = signature.params.len();
            let param_sigs = signature.param_sigs();
            let params_to_json = signature.params_to_json(parse_quote! { ClientCallMethodError });
            let receiver = &signature.receiver;
            let return_type = &signature.return_type;
            let sig_span = signature.sig_span;
            let method = quote_spanned! {sig_span=>
                async fn #impl_name(#receiver, #(#param_sigs,)*) -> #return_type {
                    let __params = if #params_len > 0 {
                        let mut __params_array = Vec::with_capacity(#params_len);
                        #(#params_to_json)*
                        Some(JsonRpcParams::Array(__params_array))
                    } else {
                        None
                    };
                    match self.client.call_method(#name, __params).await {
                        Ok(Ok(value_json)) => match value_json.try_into() {
                            Ok(value) => Ok(Ok(value)),
                            Err(source) => Err(ClientCallMethodError::ParseResponse { source }),
                        },
                        Ok(Err(json_rpc_error)) => Ok(Err(json_rpc_error.into())),
                        Err(ClientSendRequestError::Io { source }) => {
                            Err(ClientCallMethodError::Io { source })
                        },
                        Err(ClientSendRequestError::ConnectionDropped) => {
                            Err(ClientCallMethodError::ConnectionDropped)
                        },
                    }
                }
            };
            method_impls.push(method);
        }
        quote_spanned! {self.full_span=>
            #visibility struct #name<A> {
                client: JsonRpcClient<A>,
            }

            impl<A> #name<A>
            where
                A: AsyncWrite + Unpin,
            {
                pub fn from_inner(client: JsonRpcClient<A>) -> #name<A> {
                    #name { client }
                }

                pub fn into_inner(self) -> JsonRpcClient<A> {
                    self.client
                }

                #(#notification_impls)*
                #(#method_impls)*
            }
        }
    }
}

struct ClientMethodSignature {
    params: Vec<(Ident, Type)>,
    name: String,
    receiver: Receiver,
    sig_span: Span,
    return_type: Type,
}

impl ClientMethodSignature {
    fn parse_signature(
        sig: &syn::Signature,
        name: String,
    ) -> ClientMethodSignature {
        let mut receiver_opt = None;
        let mut params = Vec::with_capacity(sig.inputs.len());
        for fn_arg in &sig.inputs {
            match fn_arg {
                FnArg::Receiver(receiver) => {
                    receiver_opt = Some(receiver.clone());
                },
                FnArg::Typed(pat_type) => {
                    let ident = match &*pat_type.pat {
                        Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                        _ => abort!(pat_type.pat.span(), "cannot match on method arguments"),
                    };
                    let ty = *pat_type.ty.clone();
                    params.push((ident, ty));
                },
            }
        }
        let receiver = match receiver_opt {
            Some(receiver) => receiver,
            None => abort!(sig.span(), "expected a self parameter"),
        };
        let sig_span = sig.span();
        let return_type = match &sig.output {
            ReturnType::Default => parse_quote! { () },
            ReturnType::Type(_, return_type) => (&**return_type).clone(),
        };
        ClientMethodSignature {
            params, name, receiver, sig_span, return_type,
        }
    }

    fn param_sigs(&self) -> Vec<TokenStream> {
        let mut param_sigs = Vec::with_capacity(self.params.len());
        for (param_name, param_type) in &self.params {
            let span = param_name.span();
            let param_sig = quote_spanned! {span=>
                #param_name: #param_type
            };
            param_sigs.push(param_sig);
        }
        param_sigs
    }

    fn params_to_json(&self, error_type: Ident) -> Vec<TokenStream> {
        let mut params_to_json = Vec::with_capacity(self.params.len());
        for (param_name, _param_type) in &self.params {
            let span = param_name.span();
            let param_to_json = quote_spanned! {span=>
                let #param_name = match serde_json::to_value(#param_name) {
                    Ok(#param_name) => #param_name,
                    Err(source) => return Err(#error_type::Serialize { source }),
                };
                __params_array.push(#param_name);
            };
            params_to_json.push(param_to_json);
        }
        params_to_json
    }
}

