import { Service } from "@core/service";

export interface Props { label: string }

export function Widget(props: Props) {
    const s = new Service(props.label);
    return <div title={props.label}>{String(s.id)}</div>;
}
