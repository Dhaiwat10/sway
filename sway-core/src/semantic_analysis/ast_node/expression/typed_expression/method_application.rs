use crate::{
    declaration_engine::DeclarationId,
    error::*,
    language::{parsed::*, ty, *},
    semantic_analysis::*,
    type_system::*,
};
use ast_node::typed_expression::check_function_arguments_arity;
use std::collections::{HashMap, VecDeque};
use sway_error::error::CompileError;
use sway_types::{constants, integer_bits::IntegerBits};
use sway_types::{constants::CONTRACT_CALL_COINS_PARAMETER_NAME, Spanned};
use sway_types::{state::StateIndex, Span};

#[allow(clippy::too_many_arguments)]
pub(crate) fn type_check_method_application(
    mut ctx: TypeCheckContext,
    method_name_binding: TypeBinding<MethodName>,
    contract_call_params: Vec<StructExpressionField>,
    arguments: Vec<Expression>,
    span: Span,
) -> CompileResult<ty::TyExpression> {
    let mut warnings = vec![];
    let mut errors = vec![];

    let type_engine = ctx.type_engine;
    let declaration_engine = ctx.declaration_engine;
    let engines = ctx.engines();

    // type check the function arguments
    let mut args_buf = VecDeque::new();
    for arg in &arguments {
        let ctx = ctx
            .by_ref()
            .with_help_text("")
            .with_type_annotation(type_engine.insert_type(declaration_engine, TypeInfo::Unknown));
        args_buf.push_back(check!(
            ty::TyExpression::type_check(ctx, arg.clone()),
            ty::TyExpression::error(span.clone(), engines),
            warnings,
            errors
        ));
    }

    // resolve the method name to a typed function declaration and type_check
    let decl_id = check!(
        resolve_method_name(ctx.by_ref(), &method_name_binding, args_buf.clone()),
        return err(warnings, errors),
        warnings,
        errors
    );
    let method = check!(
        CompileResult::from(
            declaration_engine.get_function(decl_id.clone(), &method_name_binding.span())
        ),
        return err(warnings, errors),
        warnings,
        errors
    );

    // check the method visibility
    if span.path() != method.span.path() && method.visibility.is_private() {
        errors.push(CompileError::CallingPrivateLibraryMethod {
            name: method.name.as_str().to_string(),
            span,
        });
        return err(warnings, errors);
    }

    // check the function storage purity
    if !method.is_contract_call {
        // 'method.purity' is that of the callee, 'opts.purity' of the caller.
        if !ctx.purity().can_call(method.purity) {
            errors.push(CompileError::StorageAccessMismatch {
                attrs: promote_purity(ctx.purity(), method.purity).to_attribute_syntax(),
                span: method_name_binding.inner.easy_name().span(),
            });
        }
        if !contract_call_params.is_empty() {
            errors.push(CompileError::CallParamForNonContractCallMethod {
                span: contract_call_params[0].name.span(),
            });
        }
    }

    // generate the map of the contract call params
    let mut contract_call_params_map = HashMap::new();
    if method.is_contract_call {
        for param_name in &[
            constants::CONTRACT_CALL_GAS_PARAMETER_NAME,
            constants::CONTRACT_CALL_COINS_PARAMETER_NAME,
            constants::CONTRACT_CALL_ASSET_ID_PARAMETER_NAME,
        ] {
            if contract_call_params
                .iter()
                .filter(|&param| param.name.span().as_str() == *param_name)
                .count()
                > 1
            {
                errors.push(CompileError::ContractCallParamRepeated {
                    param_name: param_name.to_string(),
                    span: span.clone(),
                });
            }
        }

        for param in contract_call_params {
            match param.name.span().as_str() {
                constants::CONTRACT_CALL_GAS_PARAMETER_NAME
                | constants::CONTRACT_CALL_COINS_PARAMETER_NAME
                | constants::CONTRACT_CALL_ASSET_ID_PARAMETER_NAME => {
                    let type_annotation = type_engine.insert_type(
                        declaration_engine,
                        if param.name.span().as_str()
                            != constants::CONTRACT_CALL_ASSET_ID_PARAMETER_NAME
                        {
                            TypeInfo::UnsignedInteger(IntegerBits::SixtyFour)
                        } else {
                            TypeInfo::B256
                        },
                    );
                    let ctx = ctx
                        .by_ref()
                        .with_help_text("")
                        .with_type_annotation(type_annotation);
                    contract_call_params_map.insert(
                        param.name.to_string(),
                        check!(
                            ty::TyExpression::type_check(ctx, param.value),
                            ty::TyExpression::error(span.clone(), engines),
                            warnings,
                            errors
                        ),
                    );
                }
                _ => {
                    errors.push(CompileError::UnrecognizedContractParam {
                        param_name: param.name.to_string(),
                        span: param.name.span().clone(),
                    });
                }
            };
        }

        // check if method is non-payable but we do not know _statically_
        // the amount of coins sent in the contract call is zero
        // if the coins contract call parameter is not specified
        // it's considered to be zero and hence no error needs to be reported
        if let Some(coins_expr) = contract_call_params_map.get(CONTRACT_CALL_COINS_PARAMETER_NAME) {
            if coins_analysis::possibly_nonzero_u64_expression(
                ctx.namespace,
                declaration_engine,
                coins_expr,
            ) && !method
                .attributes
                .contains_key(&crate::transform::AttributeKind::Payable)
            {
                errors.push(CompileError::CoinsPassedToNonPayableMethod {
                    fn_name: method.name,
                    span,
                });
                return err(warnings, errors);
            }
        }
    }

    // If this method was called with self being a `StorageAccess` (e.g. storage.map.insert(..)),
    // then record the index of that storage variable and pass it on.
    let mut self_state_idx = None;
    if ctx.namespace.has_storage_declared() {
        let storage_fields = check!(
            ctx.namespace
                .get_storage_field_descriptors(declaration_engine, &span),
            return err(warnings, errors),
            warnings,
            errors
        );

        self_state_idx = match arguments.first().map(|expr| &expr.kind) {
            Some(ExpressionKind::StorageAccess(StorageAccessExpression { field_names })) => {
                let first_field = field_names[0].clone();
                let self_state_idx = match storage_fields
                    .iter()
                    .enumerate()
                    .find(|(_, ty::TyStorageField { name, .. })| name == &first_field)
                {
                    Some((ix, _)) => StateIndex::new(ix),
                    None => {
                        errors.push(CompileError::StorageFieldDoesNotExist {
                            name: first_field.clone(),
                        });
                        return err(warnings, errors);
                    }
                };
                Some(self_state_idx)
            }
            _ => None,
        }
    };

    // If this function is being called with method call syntax, a.b(c),
    // then make sure the first parameter is self, else issue an error.
    let mut is_method_call_syntax_used = false;
    if !method.is_contract_call {
        if let MethodName::FromModule { ref method_name } = method_name_binding.inner {
            is_method_call_syntax_used = true;
            let is_first_param_self = method
                .parameters
                .get(0)
                .map(|f| f.is_self())
                .unwrap_or_default();
            if !is_first_param_self {
                errors.push(CompileError::AssociatedFunctionCalledAsMethod {
                    fn_name: method_name.clone(),
                    span,
                });
                return err(warnings, errors);
            }
        }
    }

    // Validate mutability of self. Check that the variable that the method is called on is mutable
    // _if_ the method requires mutable self.
    if let (
        Some(ty::TyExpression {
            expression: ty::TyExpressionVariant::VariableExpression { name, .. },
            ..
        }),
        Some(ty::TyFunctionParameter { is_mutable, .. }),
    ) = (args_buf.get(0), method.parameters.get(0))
    {
        if *is_mutable {
            let unknown_decl = check!(
                ctx.namespace.resolve_symbol(name).cloned(),
                return err(warnings, errors),
                warnings,
                errors
            );

            let is_decl_mutable = match unknown_decl {
                ty::TyDeclaration::ConstantDeclaration(_) => false,
                _ => {
                    let variable_decl = check!(
                        unknown_decl.expect_variable().cloned(),
                        return err(warnings, errors),
                        warnings,
                        errors
                    );
                    variable_decl.mutability.is_mutable()
                }
            };

            if !is_decl_mutable {
                errors.push(CompileError::MethodRequiresMutableSelf {
                    method_name: method_name_binding.inner.easy_name(),
                    variable_name: name.clone(),
                    span,
                });
                return err(warnings, errors);
            }
        }
    }

    // retrieve the function call path
    let call_path = match method_name_binding.inner {
        MethodName::FromType {
            call_path_binding,
            method_name,
        } => {
            let prefixes =
                if let (TypeInfo::Custom { name, .. }, ..) = &call_path_binding.inner.suffix {
                    vec![name.clone()]
                } else {
                    call_path_binding.inner.prefixes
                };
            CallPath {
                prefixes,
                suffix: method_name,
                is_absolute: call_path_binding.inner.is_absolute,
            }
        }
        MethodName::FromModule { method_name } => CallPath {
            prefixes: vec![],
            suffix: method_name,
            is_absolute: false,
        },
        MethodName::FromTrait { call_path } => call_path,
    };

    // build the function selector
    let selector = if method.is_contract_call {
        let contract_caller = args_buf.pop_front();
        let contract_address =
            match contract_caller.map(|x| type_engine.look_up_type_id(x.return_type)) {
                Some(TypeInfo::ContractCaller { address, .. }) => address,
                _ => {
                    errors.push(CompileError::Internal(
                        "Attempted to find contract address of non-contract-call.",
                        span.clone(),
                    ));
                    None
                }
            };
        let contract_address = if let Some(addr) = contract_address {
            addr
        } else {
            errors.push(CompileError::ContractAddressMustBeKnown {
                span: call_path.span(),
            });
            return err(warnings, errors);
        };
        let func_selector = check!(
            method.to_fn_selector_value(type_engine),
            [0; 4],
            warnings,
            errors
        );
        Some(ty::ContractCallParams {
            func_selector,
            contract_address,
        })
    } else {
        None
    };

    // check that the number of parameters and the number of the arguments is the same
    check!(
        check_function_arguments_arity(
            args_buf.len(),
            &method,
            &call_path,
            is_method_call_syntax_used
        ),
        return err(warnings, errors),
        warnings,
        errors
    );

    // unify the types of the arguments with the types of the parameters from the function declaration
    for (arg, param) in args_buf.iter().zip(method.parameters.iter()) {
        let (mut new_warnings, new_errors) = type_engine.unify_right_with_self(
            ctx.declaration_engine,
            arg.return_type,
            param.type_id,
            ctx.self_type(),
            &arg.span,
            "This argument's type is not castable to the declared parameter type.",
        );
        warnings.append(&mut new_warnings);
        if !new_errors.is_empty() {
            errors.push(CompileError::ArgumentParameterTypeMismatch {
                span: arg.span.clone(),
                provided: engines.help_out(arg.return_type).to_string(),
                should_be: engines.help_out(param.type_id).to_string(),
            });
        }
    }

    // Map the names of the parameters to the typed arguments.
    let args_and_names = method
        .parameters
        .iter()
        .zip(args_buf.into_iter())
        .map(|(param, arg)| (param.name.clone(), arg))
        .collect::<Vec<(_, _)>>();

    let exp = ty::TyExpression {
        expression: ty::TyExpressionVariant::FunctionApplication {
            call_path,
            contract_call_params: contract_call_params_map,
            arguments: args_and_names,
            function_decl_id: decl_id,
            self_state_idx,
            selector,
        },
        return_type: method.return_type,
        span,
    };

    ok(exp, warnings, errors)
}

pub(crate) fn resolve_method_name(
    mut ctx: TypeCheckContext,
    method_name: &TypeBinding<MethodName>,
    arguments: VecDeque<ty::TyExpression>,
) -> CompileResult<DeclarationId> {
    let mut warnings = vec![];
    let mut errors = vec![];

    let type_engine = ctx.type_engine;
    let declaration_engine = ctx.declaration_engine;
    let engines = ctx.engines();

    // retrieve the function declaration using the components of the method name
    let decl_id =
        match &method_name.inner {
            MethodName::FromType {
                call_path_binding,
                method_name,
            } => {
                // type check the call path
                let type_id = check!(
                    call_path_binding.type_check_with_type_info(&mut ctx),
                    type_engine.insert_type(declaration_engine, TypeInfo::ErrorRecovery),
                    warnings,
                    errors
                );

                // find the module that the symbol is in
                let type_info_prefix = ctx
                    .namespace
                    .find_module_path(&call_path_binding.inner.prefixes);
                check!(
                    ctx.namespace.root().check_submodule(&type_info_prefix),
                    return err(warnings, errors),
                    warnings,
                    errors
                );

                // find the method
                check!(
                    ctx.namespace.find_method_for_type(
                        type_id,
                        &type_info_prefix,
                        method_name,
                        ctx.self_type(),
                        &arguments,
                        engines,
                    ),
                    return err(warnings, errors),
                    warnings,
                    errors
                )
            }
            MethodName::FromTrait { call_path } => {
                // find the module that the symbol is in
                let module_path = ctx.namespace.find_module_path(&call_path.prefixes);

                // find the type of the first argument
                let type_id = arguments.get(0).map(|x| x.return_type).unwrap_or_else(|| {
                    type_engine.insert_type(declaration_engine, TypeInfo::Unknown)
                });

                // find the method
                check!(
                    ctx.namespace.find_method_for_type(
                        type_id,
                        &module_path,
                        &call_path.suffix,
                        ctx.self_type(),
                        &arguments,
                        engines,
                    ),
                    return err(warnings, errors),
                    warnings,
                    errors
                )
            }
            MethodName::FromModule { method_name } => {
                // find the module that the symbol is in
                let module_path = ctx.namespace.find_module_path(vec![]);

                // find the type of the first argument
                let type_id = arguments.get(0).map(|x| x.return_type).unwrap_or_else(|| {
                    type_engine.insert_type(declaration_engine, TypeInfo::Unknown)
                });

                // find the method
                check!(
                    ctx.namespace.find_method_for_type(
                        type_id,
                        &module_path,
                        method_name,
                        ctx.self_type(),
                        &arguments,
                        engines,
                    ),
                    return err(warnings, errors),
                    warnings,
                    errors
                )
            }
        };

    let mut func_decl = check!(
        CompileResult::from(declaration_engine.get_function(decl_id.clone(), &decl_id.span())),
        return err(warnings, errors),
        warnings,
        errors
    );

    // monomorphize the function declaration
    check!(
        ctx.monomorphize(
            &mut func_decl,
            &mut method_name.type_arguments.clone(),
            EnforceTypeArguments::No,
            &method_name.span()
        ),
        return err(warnings, errors),
        warnings,
        errors
    );

    let decl_id = ctx
        .declaration_engine
        .insert_function(func_decl)
        .with_parent(ctx.declaration_engine, decl_id);

    ok(decl_id, warnings, errors)
}
