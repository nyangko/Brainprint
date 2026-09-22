import Parent from './Parent.svelte';
import { describeModel } from './lib/model';

export function mount(): string {
    return describeModel({ name: String(Parent), count: 0 });
}
