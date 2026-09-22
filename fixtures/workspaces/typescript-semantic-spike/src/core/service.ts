import type { Box, Id, Runner } from "./types.js";

export class Service implements Runner {
    constructor(readonly id: Id) {}
    run(): void {}
}

export function unwrap<T>(box: Box<T>): T {
    return box.value;
}
