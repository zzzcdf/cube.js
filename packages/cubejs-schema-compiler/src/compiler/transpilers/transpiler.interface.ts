export interface TranspilerInterface {
  traverseObject(): Record<string, (...args: any[]) => any>;
}
