// kernava-indexer: AST shape verification for new languages
use crate::parser::{self, Language};

fn dump_and_assert(code: &str, lang: Language, expected_kinds: &[&str], label: &str) {
    let tree = parser::parse(code, lang).unwrap();
    let sexp = tree.root_node().to_sexp();
    for kind in expected_kinds {
        assert!(
            sexp.contains(kind),
            "[{label}] expected kind '{kind}' not found in AST:\n{sexp}"
        );
    }
}

#[test]
fn dump_java_ast() {
    let code = r#"package com.example;
import java.util.List;
import com.example.math;

public class Calculator {
    private int value;
    public Calculator(int v) { this.value = v; }
    public int add(int x, int y) { return compute(x); }
    private int helper(int x) { return x * 2; }
}
interface Math { int compute(int x); }
enum Color { RED, GREEN, BLUE }
"#;
    dump_and_assert(
        code,
        Language::Java,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_declaration",
            "constructor_declaration",
            "import_declaration",
            "field_declaration",
            "method_invocation",
            "modifiers",
        ],
        "Java",
    );
}

#[test]
fn dump_csharp_ast() {
    let code = r#"using System;
using System.Collections.Generic;

namespace Example {
    public class Calculator {
        private int value;
        public Calculator(int v) { value = v; }
        public int Add(int x, int y) { return Compute(x); }
        private int Helper(int x) { return x * 2; }
    }
    public interface IMath { int Compute(int x); }
    public enum Color { Red, Green, Blue }
}
"#;
    dump_and_assert(
        code,
        Language::CSharp,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_declaration",
            "constructor_declaration",
            "using_directive",
            "namespace_declaration",
            "invocation_expression",
        ],
        "C#",
    );
}

#[test]
fn dump_ruby_ast() {
    let code = r#"require 'json'
require_relative 'helper'

class Calculator
  def initialize(value)
    @value = value
  end

  def add(x, y)
    x + y
  end

  private

  def helper(x)
    x * 2
  end
end

module Math
  def compute(x)
    x * 2
  end
end

def free_function(a, b)
  a + b
end
"#;
    dump_and_assert(
        code,
        Language::Ruby,
        &[
            "class",
            "module",
            "method",
            "method_parameters",
            "call",
            "argument_list",
            "identifier",
            "instance_variable",
        ],
        "Ruby",
    );
}

#[test]
fn dump_php_ast() {
    let code = r#"<?php
namespace Example;

use Example\Math;

class Calculator {
    private $value;
    public function __construct($v) { $this->value = $v; }
    public function add($x, $y) { return $x + $y; }
    private function helper($x) { return $x * 2; }
}

interface Math { public function compute($x); }

function free_function($a, $b) { return $a + $b; }
"#;
    dump_and_assert(
        code,
        Language::Php,
        &[
            "class_declaration",
            "interface_declaration",
            "method_declaration",
            "function_definition",
            "namespace_use_declaration",
        ],
        "PHP",
    );
}

#[test]
fn dump_c_ast() {
    let code = r#"#include <stdio.h>
#include "helper.h"

int add(int a, int b) { return a + b; }
static int helper(int x) { return x * 2; }

int main() { int r = add(1, 2); printf("%d\n", r); return 0; }
"#;
    dump_and_assert(
        code,
        Language::C,
        &[
            "function_definition",
            "declaration",
            "call_expression",
            "preproc_include",
            "identifier",
        ],
        "C",
    );
}

#[test]
fn dump_cpp_ast() {
    let code = r#"#include <iostream>
#include "helper.h"

class Calculator {
public:
    Calculator(int v) : value(v) {}
    int add(int x, int y) { return x + y; }
private:
    int value;
};

namespace math {
    int compute(int x) { return x * 2; }
}

int main() { Calculator c(1); int r = c.add(1, 2); return 0; }
"#;
    dump_and_assert(
        code,
        Language::Cpp,
        &[
            "class_specifier",
            "function_definition",
            "namespace_definition",
            "call_expression",
            "field_declaration",
            "field_expression",
        ],
        "C++",
    );
}

#[test]
fn dump_php_call_diagnostic() {
    let code = r#"<?php
class Calculator {
    public function add($x) {
        return $this->compute($x);
    }
    public function compute($x) {
        return $this->helper($x);
    }
}
"#;
    let tree = parser::parse(code, Language::Php).unwrap();
    let sexp = tree.root_node().to_sexp();
    assert!(
        sexp.contains("member_call_expression"),
        "no member_call_expression: {sexp}"
    );
}
