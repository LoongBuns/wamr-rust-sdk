/*
 * Copyright (C) 2019 Intel Corporation. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
 */

//! an exported wasm function.
//! get one via `Function::find_export_func()`

use std::{ffi::CString, marker::PhantomData};
use wamr_sys::{
    wasm_exec_env_t, wasm_func_get_param_count, wasm_func_get_result_count,
    wasm_func_get_result_types, wasm_function_inst_t, wasm_runtime_call_wasm,
    wasm_runtime_get_exception, wasm_runtime_get_exec_env_singleton,
    wasm_runtime_get_wasi_exit_code, wasm_runtime_lookup_function,
    wasm_valkind_enum_WASM_EXTERNREF, wasm_valkind_enum_WASM_F32, wasm_valkind_enum_WASM_F64,
    wasm_valkind_enum_WASM_FUNCREF, wasm_valkind_enum_WASM_I32, wasm_valkind_enum_WASM_I64,
};

use crate::{
    helper::exception_to_string, instance::Instance, value::WasmValue, ExecError, RuntimeError,
};

pub struct Function<'instance> {
    function: wasm_function_inst_t,
    _phantom: PhantomData<Instance<'instance>>,
}

impl<'instance> Function<'instance> {
    /// find a function by name
    ///
    /// # Error
    ///
    /// Return `RuntimeError::FunctionNotFound` if failed.
    pub fn find_export_func(
        instance: &'instance Instance<'instance>,
        name: &str,
    ) -> Result<Self, RuntimeError> {
        let name = CString::new(name).expect("CString::new failed");
        let function =
            unsafe { wasm_runtime_lookup_function(instance.get_inner_instance(), name.as_ptr()) };
        match function.is_null() {
            true => Err(RuntimeError::FunctionNotFound),
            false => Ok(Function {
                function,
                _phantom: PhantomData,
            }),
        }
    }

    #[allow(non_upper_case_globals)]
    fn parse_result(
        &self,
        instance: &Instance<'instance>,
        result: Vec<u32>,
    ) -> Result<Vec<WasmValue>, RuntimeError> {
        let result_count =
            unsafe { wasm_func_get_result_count(self.function, instance.get_inner_instance()) };
        if result_count == 0 {
            return Ok(vec![WasmValue::Void]);
        }

        let mut result_types = vec![0u8; result_count as usize];
        unsafe {
            wasm_func_get_result_types(
                self.function,
                instance.get_inner_instance(),
                result_types.as_mut_ptr(),
            );
        }

        let mut results = Vec::with_capacity(result_types.len());
        let mut index: usize = 0;

        for result_type in result_types.iter() {
            match *result_type as u32 {
                wasm_valkind_enum_WASM_I32
                | wasm_valkind_enum_WASM_FUNCREF
                | wasm_valkind_enum_WASM_EXTERNREF => {
                    results.push(WasmValue::decode_to_i32(vec![result[index]]));
                    index += 1;
                }
                wasm_valkind_enum_WASM_I64 => {
                    results.push(WasmValue::decode_to_i64(vec![
                        result[index],
                        result[index + 1],
                    ]));
                    index += 2;
                }
                wasm_valkind_enum_WASM_F32 => {
                    results.push(WasmValue::decode_to_f32(vec![result[index]]));
                    index += 1;
                }
                wasm_valkind_enum_WASM_F64 => {
                    results.push(WasmValue::decode_to_f64(vec![
                        result[index],
                        result[index + 1],
                    ]));
                    index += 2;
                }
                _ => return Err(RuntimeError::NotImplemented),
            }
        }

        Ok(results)
    }

    /// execute an export function.
    /// all parameters need to be wrapped in `WasmValue`
    ///
    /// # Error
    ///
    /// Return `RuntimeError::ExecutionError` if failed.
    pub fn call(
        &self,
        instance: &'instance Instance<'instance>,
        params: &Vec<WasmValue>,
    ) -> Result<Vec<WasmValue>, RuntimeError> {
        let param_count =
            unsafe { wasm_func_get_param_count(self.function, instance.get_inner_instance()) };
        if param_count > params.len() as u32 {
            return Err(RuntimeError::ExecutionError(ExecError {
                message: "invalid parameters".to_string(),
                exit_code: 0xff,
            }));
        }

        // Maintain sufficient allocated space in the vector rather than just declaring its capacity.
        let result_count =
            unsafe { wasm_func_get_result_count(self.function, instance.get_inner_instance()) };
        let capacity = std::cmp::max(param_count, result_count) as usize * 2;
        let mut argv = Vec::with_capacity(capacity);
        for p in params {
            argv.append(&mut p.encode());
        }
        argv.resize(capacity, 0);

        let call_result: bool;
        unsafe {
            let exec_env: wasm_exec_env_t =
                wasm_runtime_get_exec_env_singleton(instance.get_inner_instance());
            call_result =
                wasm_runtime_call_wasm(exec_env, self.function, param_count, argv.as_mut_ptr());
        };

        if !call_result {
            unsafe {
                let exception_c = wasm_runtime_get_exception(instance.get_inner_instance());
                let error_info = ExecError {
                    message: exception_to_string(exception_c),
                    exit_code: wasm_runtime_get_wasi_exit_code(instance.get_inner_instance()),
                };
                return Err(RuntimeError::ExecutionError(error_info));
            }
        }

        // there is no out of bounds problem, because we precalculated the safe vec size
        self.parse_result(instance, argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{module::Module, runtime::Runtime, wasi_context::WasiCtxBuilder};
    use std::path::PathBuf;

    #[test]
    fn test_func_in_wasm32_unknown() {
        let runtime = Runtime::new().unwrap();

        // (module
        //   (func (export "add") (param i64 i32) (result i32 i64)
        //     (local.get 1)
        //     (i32.const 32)
        //     (i32.add)
        //     (local.get 0)
        //     (i64.const 64)
        //     (i64.add)
        //   )
        //
        //   (func (export "multi-result") (result i32 i64 i32)
        //     (i32.const 1)
        //     (i64.const 2)
        //     (i32.const 3)
        //   )
        // )
        let binary = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x0E, 0x02, 0x60, 0x02, 0x7E,
            0x7F, 0x02, 0x7F, 0x7E, 0x60, 0x00, 0x03, 0x7F, 0x7E, 0x7F, 0x03, 0x03, 0x02, 0x00,
            0x01, 0x07, 0x16, 0x02, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x0C, 0x6D, 0x75, 0x6C,
            0x74, 0x69, 0x2D, 0x72, 0x65, 0x73, 0x75, 0x6C, 0x74, 0x00, 0x01, 0x0A, 0x18, 0x02,
            0x0D, 0x00, 0x20, 0x01, 0x41, 0x20, 0x6A, 0x20, 0x00, 0x42, 0xC0, 0x00, 0x7C, 0x0B,
            0x08, 0x00, 0x41, 0x01, 0x42, 0x02, 0x41, 0x03, 0x0B,
        ];
        let binary = binary.into_iter().map(|c| c as u8).collect::<Vec<u8>>();

        let module = Module::from_vec(&runtime, binary, "");
        assert!(module.is_ok());
        let module = module.unwrap();

        let instance = Instance::new(&runtime, &module, 1024);
        assert!(instance.is_ok());
        let instance: &Instance = &instance.unwrap();

        //
        // run add()
        //

        let function = Function::find_export_func(instance, "add");
        assert!(function.is_ok());
        let function = function.unwrap();

        let params: Vec<WasmValue> = vec![WasmValue::I64(10), WasmValue::I32(20)];
        let call_result = function.call(instance, &params);
        assert!(call_result.is_ok());
        assert_eq!(
            call_result.unwrap(),
            vec![WasmValue::I32(52), WasmValue::I64(74)]
        );

        //
        // run multi-result()
        //

        let function = Function::find_export_func(instance, "multi-result");
        assert!(function.is_ok());
        let function = function.unwrap();

        let params: Vec<WasmValue> = Vec::new();
        let call_result = function.call(instance, &params);
        assert!(call_result.is_ok());
        assert_eq!(
            call_result.unwrap(),
            vec![WasmValue::I32(1), WasmValue::I64(2), WasmValue::I32(3)]
        );
    }

    #[test]
    fn test_func_in_wasm32_wasi() {
        let runtime = Runtime::new().unwrap();

        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test");
        d.push("gcd_wasm32_wasi.wasm");
        let module = Module::from_file(&runtime, d.as_path());
        assert!(module.is_ok());
        let mut module = module.unwrap();

        let wasi_ctx = WasiCtxBuilder::new()
            .set_pre_open_path(vec!["."], vec![])
            .build();
        module.set_wasi_context(wasi_ctx);

        let instance = Instance::new(&runtime, &module, 1024 * 64);
        assert!(instance.is_ok());
        let instance: &Instance = &instance.unwrap();

        let function = Function::find_export_func(instance, "gcd");
        assert!(function.is_ok());
        let function = function.unwrap();

        let params: Vec<WasmValue> = vec![WasmValue::I32(9), WasmValue::I32(27)];
        let result = function.call(instance, &params);
        assert_eq!(result.unwrap(), vec![WasmValue::I32(9)]);

        let params: Vec<WasmValue> = vec![WasmValue::I32(0), WasmValue::I32(27)];
        let result = function.call(instance, &params);
        assert_eq!(result.unwrap(), vec![WasmValue::I32(27)]);
    }

    #[test]
    fn test_func_in_wasm32_wasi_w_args() {
        let runtime = Runtime::new().unwrap();

        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test");
        d.push("wasi-demo-app.wasm");
        let module = Module::from_file(&runtime, d.as_path());
        assert!(module.is_ok());
        let mut module = module.unwrap();

        let wasi_ctx = WasiCtxBuilder::new()
            .set_pre_open_path(vec!["."], vec![])
            .set_arguments(vec!["wasi-demo-app.wasm", "echo", "hi"])
            .build();
        module.set_wasi_context(wasi_ctx);

        let instance = Instance::new(&runtime, &module, 1024 * 64);
        assert!(instance.is_ok());
        let instance: &Instance = &instance.unwrap();

        let function = Function::find_export_func(instance, "_start");
        assert!(function.is_ok());
        let function = function.unwrap();

        let result = function.call(instance, &vec![]);
        assert!(result.is_ok());
        println!("{:?}", result.unwrap());
    }
}
