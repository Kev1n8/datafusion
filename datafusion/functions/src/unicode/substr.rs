// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::any::Any;
use std::cmp::max;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayAccessor, ArrayIter, ArrayRef, AsArray, GenericStringArray,
    OffsetSizeTrait, StringViewArray, StringViewBuilder,
};
use arrow::datatypes::DataType;

use datafusion_common::cast::as_int64_array;
use datafusion_common::{exec_err, Result};
use datafusion_expr::TypeSignature::Exact;
use datafusion_expr::{ColumnarValue, ScalarUDFImpl, Signature, Volatility};

use crate::utils::{make_scalar_function, utf8_to_str_type};

#[derive(Debug)]
pub struct SubstrFunc {
    signature: Signature,
    aliases: Vec<String>,
}

impl Default for SubstrFunc {
    fn default() -> Self {
        Self::new()
    }
}

impl SubstrFunc {
    pub fn new() -> Self {
        use DataType::*;
        Self {
            signature: Signature::one_of(
                vec![
                    Exact(vec![Utf8, Int64]),
                    Exact(vec![LargeUtf8, Int64]),
                    Exact(vec![Utf8, Int64, Int64]),
                    Exact(vec![LargeUtf8, Int64, Int64]),
                    Exact(vec![Utf8View, Int64]),
                    Exact(vec![Utf8View, Int64, Int64]),
                ],
                Volatility::Immutable,
            ),
            aliases: vec![String::from("substring")],
        }
    }
}

impl ScalarUDFImpl for SubstrFunc {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "substr"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        if arg_types[0] == DataType::Utf8View {
            Ok(DataType::Utf8View)
        } else {
            utf8_to_str_type(&arg_types[0], "substr")
        }
    }

    fn invoke(&self, args: &[ColumnarValue]) -> Result<ColumnarValue> {
        make_scalar_function(substr, vec![])(args)
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }
}

/// Extracts the substring of string starting at the start'th character, and extending for count characters if that is specified. (Same as substring(string from start for count).)
/// substr('alphabet', 3) = 'phabet'
/// substr('alphabet', 3, 2) = 'ph'
/// The implementation uses UTF-8 code points as characters
pub fn substr(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args[0].data_type() {
        DataType::Utf8 => {
            let string_array = args[0].as_string::<i32>();
            string_substr::<_, i32>(string_array, &args[1..])
        }
        DataType::LargeUtf8 => {
            let string_array = args[0].as_string::<i64>();
            string_substr::<_, i64>(string_array, &args[1..])
        }
        DataType::Utf8View => {
            let string_array = args[0].as_string_view();
            string_view_substr(string_array, &args[1..])
        }
        other => exec_err!(
            "Unsupported data type {other:?} for function substr,\
            expected Utf8View, Utf8 or LargeUtf8."
        ),
    }
}

// Return the exact byte index for [start, end), set count to -1 to ignore count
fn get_true_start_count(input: &str, start: usize, count: i64) -> (usize, usize) {
    let (mut st, mut ed) = (input.len(), input.len());
    let mut start_counting = false;
    let mut cnt = 0;
    for (char_cnt, (byte_cnt, _)) in input.char_indices().enumerate() {
        if char_cnt == start {
            st = byte_cnt;
            if count != -1 {
                start_counting = true;
            } else {
                break;
            }
        }
        if start_counting {
            if cnt == count {
                ed = byte_cnt;
                break;
            }
            cnt += 1;
        }
    }
    (st, ed)
}

// The decoding process refs the trait at: arrow/arrow-data/src/byte_view.rs:44
// From<u128> for ByteView
fn string_view_substr(
    string_array: &StringViewArray,
    args: &[ArrayRef],
) -> Result<ArrayRef> {
    let mut builder = StringViewBuilder::new();
    // Copy all blocks from input
    for block in string_array.data_buffers() {
        builder.append_block(block.clone());
    }

    let start_array = as_int64_array(&args[0])?;

    match args.len() {
        1 => {
            for (idx, (raw, start)) in string_array
                .views()
                .iter()
                .zip(start_array.iter())
                .enumerate()
            {
                if let Some(start) = start {
                    let length = *raw as u32;
                    let start = (start - 1).max(0);

                    // Operate according to the length of bytes
                    if length == 0 {
                        builder.append_null();
                    } else if length > 12 {
                        let buffer_index = (*raw >> 64) as u32;
                        let offset = (*raw >> 96) as u32;
                        // Safety:
                        // 1. idx < string_array.views.size()
                        // 2. builder is guaranteed to have corresponding blocks
                        unsafe {
                            let str = string_array.value_unchecked(idx);
                            let (start, end) =
                                get_true_start_count(str, start as usize, -1);
                            builder.append_view_unchecked(
                                buffer_index,
                                offset + start as u32,
                                // guarantee that end-offset >= 0 for end <= str.len()
                                (end - start) as u32,
                            );
                        }
                    } else {
                        // Safety:
                        // (1) original bytes are valid utf-8,
                        // (2) we do not slice on utf-8 codepoint
                        unsafe {
                            let bytes =
                                StringViewArray::inline_value(raw, length as usize);
                            let str =
                                std::str::from_utf8_unchecked(&bytes[..length as usize]);
                            // Extract str[start, end) by char
                            let (start, end) =
                                get_true_start_count(str, start as usize, length as i64);
                            builder.append_value(&str[start..end]);
                        }
                    }
                } else {
                    builder.append_null();
                }
            }
        }
        2 => {
            let count_array = as_int64_array(&args[1])?;
            for (idx, ((raw, start), count)) in string_array
                .views()
                .iter()
                .zip(start_array.iter())
                .zip(count_array.iter())
                .enumerate()
            {
                if let (Some(start), Some(count)) = (start, count) {
                    let length = *raw as u32;
                    let start = start.saturating_sub(1) as usize;
                    if count < 0 {
                        return exec_err!(
                            "negative substring length not allowed: substr(<str>, {start}, {count})"
                        );
                    } else {
                        let count = (count as u32).min(length);
                        if length == 0 {
                            builder.append_null();
                        } else if length > 12 {
                            let buffer_index = (*raw >> 64) as u32;
                            let offset = (*raw >> 96) as u32;
                            // Safety:
                            // 1. idx < string_array.views.size()
                            // 2. builder is guaranteed to have corresponding blocks
                            unsafe {
                                let str = string_array.value_unchecked(idx);
                                let (start, end) =
                                    get_true_start_count(str, start, count as i64);
                                builder.append_view_unchecked(
                                    buffer_index,
                                    offset + start as u32,
                                    // guarantee that end-offset >= 0 for end <= str.len()
                                    (end - start) as u32,
                                );
                            }
                        } else {
                            // Safety:
                            // (1) original bytes are valid utf-8,
                            // (2) we do not slice on utf-8 codepoint
                            unsafe {
                                let bytes =
                                    StringViewArray::inline_value(raw, length as usize);
                                let str = std::str::from_utf8_unchecked(
                                    &bytes[..length as usize],
                                );
                                // Extract str[start, end) by char
                                let (start, end) =
                                    get_true_start_count(str, start, count as i64);
                                builder.append_value(&str[start..end]);
                            }
                        }
                    }
                } else {
                    builder.append_null();
                }
            }
        }
        other => {
            return exec_err!(
                "substr was called with {other} arguments. It requires 2 or 3."
            )
        }
    }

    let result = builder.finish();
    Ok(Arc::new(result) as ArrayRef)
}

fn string_substr<'a, V, T>(string_array: V, args: &[ArrayRef]) -> Result<ArrayRef>
where
    V: ArrayAccessor<Item = &'a str>,
    T: OffsetSizeTrait,
{
    match args.len() {
        1 => {
            let iter = ArrayIter::new(string_array);
            let start_array = as_int64_array(&args[0])?;

            let result = iter
                .zip(start_array.iter())
                .map(|(string, start)| match (string, start) {
                    (Some(string), Some(start)) => {
                        if start <= 0 {
                            Some(string.to_string())
                        } else {
                            Some(string.chars().skip(start as usize - 1).collect())
                        }
                    }
                    _ => None,
                })
                .collect::<GenericStringArray<T>>();
            Ok(Arc::new(result) as ArrayRef)
        }
        2 => {
            let iter = ArrayIter::new(string_array);
            let start_array = as_int64_array(&args[0])?;
            let count_array = as_int64_array(&args[1])?;

            let result = iter
                .zip(start_array.iter())
                .zip(count_array.iter())
                .map(|((string, start), count)| match (string, start, count) {
                    (Some(string), Some(start), Some(count)) => {
                        if count < 0 {
                            exec_err!(
                                "negative substring length not allowed: substr(<str>, {start}, {count})"
                            )
                        } else {
                            let skip = max(0, start - 1);
                            let count = max(0, count + (if start < 1 {start - 1} else {0}));
                            Ok(Some(string.chars().skip(skip as usize).take(count as usize).collect::<String>()))
                        }
                    }
                    _ => Ok(None),
                })
                .collect::<Result<GenericStringArray<T>>>()?;

            Ok(Arc::new(result) as ArrayRef)
        }
        other => {
            exec_err!("substr was called with {other} arguments. It requires 2 or 3.")
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, StringArray, StringViewArray};
    use arrow::datatypes::DataType::{Utf8, Utf8View};

    use datafusion_common::{exec_err, Result, ScalarValue};
    use datafusion_expr::{ColumnarValue, ScalarUDFImpl};

    use crate::unicode::substr::SubstrFunc;
    use crate::utils::test::test_function;

    #[test]
    fn test_functions() -> Result<()> {
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(None)),
                ColumnarValue::Scalar(ScalarValue::from(1i64)),
            ],
            Ok(None),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(0i64)),
            ],
            Ok(Some("alphabet")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "this és longer than 12B"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
                ColumnarValue::Scalar(ScalarValue::from(2i64)),
            ],
            Ok(Some(" é")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "this is longer than 12B"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
            ],
            Ok(Some(" is longer than 12B")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "joséésoj"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
            ],
            Ok(Some("ésoj")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
                ColumnarValue::Scalar(ScalarValue::from(2i64)),
            ],
            Ok(Some("ph")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::Utf8View(Some(String::from(
                    "alphabet"
                )))),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
                ColumnarValue::Scalar(ScalarValue::from(20i64)),
            ],
            Ok(Some("phabet")),
            &str,
            Utf8View,
            StringViewArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(0i64)),
            ],
            Ok(Some("alphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("joséésoj")),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
            ],
            Ok(Some("ésoj")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("joséésoj")),
                ColumnarValue::Scalar(ScalarValue::from(-5i64)),
            ],
            Ok(Some("joséésoj")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(1i64)),
            ],
            Ok(Some("alphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(2i64)),
            ],
            Ok(Some("lphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
            ],
            Ok(Some("phabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(-3i64)),
            ],
            Ok(Some("alphabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(30i64)),
            ],
            Ok(Some("")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::Int64(None)),
            ],
            Ok(None),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
                ColumnarValue::Scalar(ScalarValue::from(2i64)),
            ],
            Ok(Some("ph")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
                ColumnarValue::Scalar(ScalarValue::from(20i64)),
            ],
            Ok(Some("phabet")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(0i64)),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
            ],
            Ok(Some("alph")),
            &str,
            Utf8,
            StringArray
        );
        // starting from 5 (10 + -5)
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(-5i64)),
                ColumnarValue::Scalar(ScalarValue::from(10i64)),
            ],
            Ok(Some("alph")),
            &str,
            Utf8,
            StringArray
        );
        // starting from -1 (4 + -5)
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(-5i64)),
                ColumnarValue::Scalar(ScalarValue::from(4i64)),
            ],
            Ok(Some("")),
            &str,
            Utf8,
            StringArray
        );
        // starting from 0 (5 + -5)
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(-5i64)),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
            ],
            Ok(Some("")),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::Int64(None)),
                ColumnarValue::Scalar(ScalarValue::from(20i64)),
            ],
            Ok(None),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(3i64)),
                ColumnarValue::Scalar(ScalarValue::Int64(None)),
            ],
            Ok(None),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(1i64)),
                ColumnarValue::Scalar(ScalarValue::from(-1i64)),
            ],
            exec_err!("negative substring length not allowed: substr(<str>, 1, -1)"),
            &str,
            Utf8,
            StringArray
        );
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("joséésoj")),
                ColumnarValue::Scalar(ScalarValue::from(5i64)),
                ColumnarValue::Scalar(ScalarValue::from(2i64)),
            ],
            Ok(Some("és")),
            &str,
            Utf8,
            StringArray
        );
        #[cfg(not(feature = "unicode_expressions"))]
        test_function!(
            SubstrFunc::new(),
            &[
                ColumnarValue::Scalar(ScalarValue::from("alphabet")),
                ColumnarValue::Scalar(ScalarValue::from(0i64)),
            ],
            internal_err!(
                "function substr requires compilation with feature flag: unicode_expressions."
            ),
            &str,
            Utf8,
            StringArray
        );

        Ok(())
    }
}
