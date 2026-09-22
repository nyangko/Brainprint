export interface Runner {
    run(): void;
}

export interface Box<T> {
    value: T;
}

export type Id = string | number;
