import { PublicModel } from "./public.js";
import { Service, unwrap } from "@core/service";
import type { Box } from "@core/types";
import type { Id } from "./public.js";

export function tag(id: Id): string {
    return String(id);
}

export function consume(): string {
    const m = new PublicModel();
    const s = new Service(1);
    s.run();
    const b: Box<string> = { value: "x" };
    return m.describe() + unwrap(b);
}
