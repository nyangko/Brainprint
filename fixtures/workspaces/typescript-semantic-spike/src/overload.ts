export interface StringResult { s: string }
export interface NumberResult { n: number }

export function parse(value: string): StringResult;
export function parse(value: number): NumberResult;
export function parse(value: string | number): StringResult | NumberResult {
    return typeof value === "string" ? { s: value } : { n: value };
}

export const x = parse("a");
export const y = parse(1);
