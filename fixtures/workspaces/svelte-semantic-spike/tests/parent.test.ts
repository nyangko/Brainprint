import { describeModel } from '../src/lib/model';

export function testDescribe(): boolean {
    return describeModel({ name: 't', count: 1 }).length > 0;
}
