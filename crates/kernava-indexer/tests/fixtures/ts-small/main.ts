import { add, multiply } from './math';
import { helper } from './util';

export function main(): number {
  const sum = add(1, 2);
  const product = multiply(3, 4);
  const result = helper("hello world example");

  const arr: number[] = [1, 2, 3];
  arr.push(sum);

  return result.length + sum + product;
}
