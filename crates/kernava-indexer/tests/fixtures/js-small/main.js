const { add, multiply } = require('./math');
const { helper } = require('./util');

function main() {
  const sum = add(1, 2);
  const product = multiply(3, 4);
  const result = helper("hello world example");
  return result.length + sum + product;
}

module.exports = { main };
