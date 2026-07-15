import { a_func } from './aaa';

export function b_value(): number {
  return 42;
}

export function b_uses_a(): number {
  return a_func();
}
