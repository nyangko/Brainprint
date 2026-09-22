import type { Runner } from "./core/types.js";

export class Base {
    run(): void {}
}

export class Child extends Base implements Runner {
    override run(): void {}
}

export function invoke(b: Beta): void {
    b.run();
}

export class Alpha { run(): void {} }
export class Beta { run(): void {} }
export class Gamma { run(): void {} }
