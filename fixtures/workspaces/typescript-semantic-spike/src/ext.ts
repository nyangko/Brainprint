import { join } from "path";
import { Buffer } from "buffer";

export function ext(parts: string[]): string {
    return join(...parts) + Buffer.from("x").toString();
}
