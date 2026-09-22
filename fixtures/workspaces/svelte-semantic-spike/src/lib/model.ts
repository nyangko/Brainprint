export interface Model {
    name: string;
    count: number;
}

export function describeModel(model: Model): string {
    return `${model.name}:${model.count}`;
}
