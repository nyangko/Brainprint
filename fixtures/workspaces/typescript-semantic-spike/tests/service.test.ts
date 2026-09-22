import { Service } from "@core/service";
import { consume } from "../src/consumer.js";

export function testService(): boolean {
    const s = new Service("t");
    s.run();
    return consume().length > 0;
}
