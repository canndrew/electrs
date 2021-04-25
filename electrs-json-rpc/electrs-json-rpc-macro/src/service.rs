use super::*;

struct ServiceSignature {
    params: Vec<(Ident, Type)>,
    method_impl_name: Ident,
    is_static: bool,
    is_async: bool,
    attr_span: Span,
}

impl ServiceSignature {
    fn parse_signature<'i>(
        sig: &syn::Signature,
        attr_span: Span,
    ) -> ServiceSignature {
        let is_async = sig.asyncness.is_some();
        let mut is_static = true;
        let mut params = Vec::with_capacity(sig.inputs.len());
        for fn_arg in sig.inputs.iter() {
            match fn_arg {
                FnArg::Receiver(receiver) => {
                    if receiver.reference.is_none() {
                        abort!(fn_arg.span(), "self must be taken by reference");
                    }
                    if receiver.mutability.is_some() {
                        abort!(fn_arg.span(), "self reference must be immutable");
                    }
                    is_static = false;
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
        let method_impl_name = sig.ident.clone();
        ServiceSignature {
            params,
            method_impl_name,
            is_static,
            is_async,
            attr_span,
        }
    }

    fn param_names(&self) -> Vec<&Ident> {
        self
        .params
        .iter()
        .map(|(name, _ty)| name)
        .collect()
    }

    fn params_from_json(&self, method_call: bool) -> Vec<TokenStream> {
        let mut params_from_json = Vec::with_capacity(self.params.len());
        for (name, ty) in &self.params {
            let span = ty.span();
            let on_error = if method_call {
                quote_spanned! {span=>
                    Err(HandleMethodError::InvalidParams {
                        message: format!("parameter {} malformed", stringify!(#name)),
                        data: Some(JsonValue::String(format!("{}", err))),
                    })
                }
            } else {
                quote_spanned! {span=>
                    drop(err)
                }
            };
            let param_from_json = quote_spanned! {span=>
                let #name: #ty = match serde_json::from_value(#name) {
                    Ok(#name) => #name,
                    Err(err) => return #on_error,
                };
            };
            params_from_json.push(param_from_json);
        }
        params_from_json
    }

    fn call_syn(&self, self_ty: &Type) -> TokenStream {
        let attr_span = self.attr_span;
        let param_names = self.param_names();
        let method_impl_name = &self.method_impl_name;
        let call = if self.is_static {
            quote_spanned! {attr_span=>
                <#self_ty>::#method_impl_name(#(#param_names,)*)
            }
        } else {
            quote_spanned! {attr_span=>
                self.#method_impl_name(#(#param_names,)*)
            }
        };
        if self.is_async {
            quote_spanned! {attr_span=>
                #call.await
            }
        } else {
            call
        }
    }

    fn call_with_array(&self, self_ty: &Type, method_call: bool) -> TokenStream {
        let attr_span = self.attr_span;
        let mut params_from_array = Vec::with_capacity(self.params.len());
        for (name, _ty) in &self.params {
            let param_from_array = quote_spanned! {attr_span=>
                let #name: JsonValue = __array_values_iter.next().unwrap();
            };
            params_from_array.push(param_from_array);
        }
        let params_from_json = self.params_from_json(method_call);
        let call = self.call_syn(self_ty);
        quote_spanned! {attr_span=> {
            let mut __array_values_iter = __array_values.into_iter();
            #(#params_from_array)*
            #(#params_from_json)*
            #call
        }}
    }

    fn call_with_object(&self, self_ty: &Type, method_call: bool) -> TokenStream {
        let attr_span = self.attr_span;
        let mut params_from_object = Vec::with_capacity(self.params.len());
        for (name, _ty) in &self.params {
            let on_error = if method_call {
                quote_spanned! {attr_span=>
                    Err(HandleMethodError::InvalidParams {
                        message: format!("parameter {} missing", stringify!(#name)),
                        data: None,
                    })
                }
            } else {
                quote_spanned! {attr_span=>
                    ()
                }
            };
            let param_from_object = quote_spanned! {attr_span=>
                let #name: JsonValue = match __object_values.remove(stringify!(#name)) {
                    Some(value) => value,
                    None => return #on_error,
                };
            };
            params_from_object.push(param_from_object);
        }
        let params_from_json = self.params_from_json(method_call);
        let call = self.call_syn(self_ty);
        quote_spanned! {attr_span=> {
            #(#params_from_object)*
            #(#params_from_json)*
            #call
        }}
    }
}

struct ServiceMethodSignature {
    signature: ServiceSignature,
    return_type: Type,
    return_span: Span,
}

impl ServiceMethodSignature {
    fn serialize_result(&self) -> TokenStream {
        let return_span = self.return_span;
        quote_spanned! {return_span=>
            match __method_call_result {
                Ok(value) => {
                    match serde_json::to_value(value) {
                        Ok(value) => Ok(value),
                        Err(err) => {
                            return Err(HandleMethodError::InternalError {
                                message: format!("json serialization of return value failed"),
                                data: Some(JsonValue::String(format!("{}", err))),
                            });
                        },
                    }
                },
                Err(err) => {
                    Err(HandleMethodError::ApplicationError(
                        JsonRpcError::from(err),
                    ))
                },
            }
        }
    }

    fn call_with_array(&self, self_ty: &Type) -> TokenStream {
        let return_type = &self.return_type;
        let attr_span = self.signature.attr_span;
        let signature_call_with_array = self.signature.call_with_array(self_ty, true);
        let serialize_result = self.serialize_result();
        quote_spanned! {attr_span=> {
            let __method_call_result: #return_type = #signature_call_with_array;
            #serialize_result
        }}
    }

    fn call_with_object(&self, self_ty: &Type) -> TokenStream {
        let return_type = &self.return_type;
        let attr_span = self.signature.attr_span;
        let signature_call_with_object = self.signature.call_with_object(self_ty, true);
        let serialize_result = self.serialize_result();
        quote_spanned! {attr_span=> {
            let __method_call_result: #return_type = #signature_call_with_object;
            #serialize_result
        }}
    }

    fn call_syn(&self, self_ty: &Type) -> TokenStream {
        let return_type = &self.return_type;
        let attr_span = self.signature.attr_span;
        let signature_call_syn = self.signature.call_syn(self_ty);
        let serialize_result = self.serialize_result();
        quote_spanned! {attr_span=> {
            let __method_call_result: #return_type = #signature_call_syn;
            #serialize_result
        }}
    }
}

pub struct JsonRpcServiceImpl {
    methods: HashMap<String, HashMap<usize, ServiceMethodSignature>>,
    notifications: HashMap<String, HashMap<usize, ServiceSignature>>,
    self_ty: Type,
    generics: Generics,
    top_attr_span: Span,
}

impl JsonRpcServiceImpl {
    pub fn parse_impl(item_impl: &mut ItemImpl) -> JsonRpcServiceImpl {
        let mut methods: HashMap<String, HashMap<usize, ServiceMethodSignature>> = HashMap::new();
        let mut notifications: HashMap<String, HashMap<usize, ServiceSignature>> = HashMap::new();
        for impl_item in &mut item_impl.items {
            let impl_item_method = match impl_item {
                ImplItem::Method(impl_item_method) => impl_item_method,
                _ => continue,
            };
            let attr = {
                let index_opt = impl_item_method.attrs.iter().position(|attr| {
                    attr.path.is_ident("method") || attr.path.is_ident("notification")
                });
                match index_opt {
                    Some(index) => impl_item_method.attrs.remove(index),
                    None => continue,
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
                                    "method name must be a string",
                                )
                            },
                        }
                    },
                    _ => {
                        abort!(
                            attr.span(),
                            "Invalid use of attribute";
                            help = "Syntax is #[method = \"method_name\"]",
                        )
                    },
                };
                let methods_of_arity = methods.entry(name.clone()).or_default();
                let arity = match impl_item_method.sig.receiver() {
                    None => impl_item_method.sig.inputs.len(),
                    Some(_) => impl_item_method.sig.inputs.len() - 1,
                };
                let signature = ServiceSignature::parse_signature(&impl_item_method.sig, attr.span());
                let return_type = match &impl_item_method.sig.output {
                    ReturnType::Default => parse_quote! { () },
                    ReturnType::Type(_, return_type) => (&**return_type).clone(),
                };
                let return_span = impl_item_method.sig.output.span();
                let method = ServiceMethodSignature {
                    signature, return_type, return_span,
                };
                match methods_of_arity.insert(arity, method) {
                    Some(prev_method) => {
                        abort!(
                            attr.span(),
                            "multiple overrides of method {} with arity {}",
                            name,
                            arity;
                            note = prev_method.signature.attr_span => "previous override here",
                        );
                    },
                    None => ()
                };
            } else if attr.path.is_ident("notification") {
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
                            help = "Syntax is #[notification = \"notification_name\"]",
                        )
                    },
                };
                let notifications_of_arity = notifications.entry(name.clone()).or_default();
                let arity = match impl_item_method.sig.receiver() {
                    None => impl_item_method.sig.inputs.len(),
                    Some(_) => impl_item_method.sig.inputs.len() - 1,
                };
                let signature = ServiceSignature::parse_signature(&impl_item_method.sig, attr.span());
                match notifications_of_arity.insert(arity, signature) {
                    Some(prev_notification) => {
                        abort!(
                            attr.span(),
                            "multiple overrides of notification {} with arity {}",
                            name,
                            arity;
                            note = prev_notification.attr_span => "previous override here",
                        );
                    },
                    None => ()
                };
            } else {
                unreachable!()
            }
        }
        let self_ty = (&*item_impl.self_ty).clone();
        let generics = item_impl.generics.clone();
        let top_attr_span = item_impl.span();
        JsonRpcServiceImpl { methods, notifications, self_ty, generics, top_attr_span }
    }

    pub fn into_syn(&self) -> TokenStream {
        let top_attr_span = self.top_attr_span;
        let mut method_branches = Vec::with_capacity(self.methods.len());
        for (name, methods_of_arity) in &self.methods {
            let no_params = match methods_of_arity.get(&0) {
                Some(method_impl) => method_impl.call_syn(&self.self_ty),
                None => quote_spanned! {top_attr_span=>
                    return Err(HandleMethodError::InvalidParams {
                        message: format!("missing parameters"),
                        data: None,
                    })
                },
            };
            let mut calls_with_array = Vec::with_capacity(methods_of_arity.len());
            let mut calls_with_object = Vec::with_capacity(methods_of_arity.len());
            for (arity, method_impl) in methods_of_arity {
                let attr_span = method_impl.signature.attr_span;
                let method_impl_call_with_array = method_impl.call_with_array(&self.self_ty);
                let method_impl_call_with_object = method_impl.call_with_object(&self.self_ty);
                let call_with_array = quote_spanned! {attr_span=>
                    #arity => #method_impl_call_with_array
                };
                let call_with_object = quote_spanned! {attr_span=>
                    #arity => #method_impl_call_with_object
                };
                calls_with_array.push(call_with_array);
                calls_with_object.push(call_with_object);
            };
            let method_branch = quote_spanned! {top_attr_span=>
                #name => {
                    match __params {
                        None => #no_params,
                        Some(JsonRpcParams::Array(__array_values)) => {
                            match __array_values.len() {
                                #(#calls_with_array,)*
                                _ => {
                                    return Err(HandleMethodError::InvalidParams {
                                        message: format!("invalid number of parameters"),
                                        data: None,
                                    });
                                },
                            }
                        },
                        Some(JsonRpcParams::Object(mut __object_values)) => {
                            match __object_values.len() {
                                #(#calls_with_object,)*
                                _ => {
                                    return Err(HandleMethodError::InvalidParams {
                                        message: format!("invalid number of parameters"),
                                        data: None,
                                    });
                                },
                            }
                        },
                    }
                }
            };
            method_branches.push(method_branch);
        }
        let mut notification_branches = Vec::with_capacity(self.methods.len());
        for (name, notifications_of_arity) in &self.notifications {
            let no_params = match notifications_of_arity.get(&0) {
                Some(signature) => signature.call_syn(&self.self_ty),
                None => quote_spanned! {top_attr_span=> () }
            };
            let mut calls_with_array = Vec::with_capacity(notifications_of_arity.len());
            let mut calls_with_object = Vec::with_capacity(notifications_of_arity.len());
            for (arity, signature) in notifications_of_arity {
                let attr_span = signature.attr_span;
                let signature_call_with_array = signature.call_with_array(&self.self_ty, false);
                let signature_call_with_object = signature.call_with_object(&self.self_ty, false);
                let call_with_array = quote_spanned! {attr_span=>
                    #arity => #signature_call_with_array
                };
                let call_with_object = quote_spanned! {attr_span=>
                    #arity => #signature_call_with_object
                };
                calls_with_array.push(call_with_array);
                calls_with_object.push(call_with_object);
            };
            let notification_branch = quote_spanned! {top_attr_span=>
                #name => {
                    match __params {
                        None => #no_params,
                        Some(JsonRpcParams::Array(__array_values)) => {
                            match __array_values.len() {
                                #(#calls_with_array,)*
                                _ => (),
                            }
                        },
                        Some(JsonRpcParams::Object(mut __object_values)) => {
                            match __object_values.len() {
                                #(#calls_with_object,)*
                                _ => (),
                            }
                        },
                    }
                }
            };
            notification_branches.push(notification_branch);
        }
        let self_ty = &self.self_ty;
        let (impl_generics, _type_generics, where_clause_opt) = self.generics.split_for_impl();
        quote_spanned! {top_attr_span=>
            #[async_trait]
            impl #impl_generics JsonRpcService for #self_ty
            #where_clause_opt
            {
                async fn handle_method<'s, 'm>(
                    &'s self,
                    __method: &'m str,
                    __params: Option<JsonRpcParams>,
                )
                    -> Result<JsonValue, HandleMethodError>
                {
                    match __method {
                        #(#method_branches,)*
                        _ => Err(HandleMethodError::MethodNotFound),
                    }
                }

                async fn handle_notification<'s, 'm>(
                    &'s self,
                    __method: &'m str,
                    __params: Option<JsonRpcParams>,
                ) {
                    match __method {
                        #(#notification_branches,)*
                        _ => (),
                    }
                }
            }
        }
    }
}

