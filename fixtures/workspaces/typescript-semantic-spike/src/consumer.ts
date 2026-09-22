import { PublicModel } from "./public.js";
import { Service, unwrap } from "@core/service";
import type { Box } from "@core/types";

export function consume(): string {
    const m = new PublicModel();
    const s = new Service(1);
    s.run();
    const b: Box<string> = { value: "x" };
    return m.describe() + unwrap(b);
}
