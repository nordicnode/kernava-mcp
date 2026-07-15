<?php
namespace Example;

use Example\Math;

class Calculator {
    private $value;

    public function __construct($v) {
        $this->value = $v;
    }

    public function add($x, $y) {
        return $this->compute($x);
    }

    private function helper($x) {
        return $x * 2;
    }

    public function compute($x) {
        return $this->helper($x) + $this->value;
    }
}

interface Math {
    public function compute($x);
}

function free_function($a, $b) {
    return $a + $b;
}
