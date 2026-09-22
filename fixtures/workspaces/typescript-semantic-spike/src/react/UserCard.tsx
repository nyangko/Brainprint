export interface UserCardProps {
    name: string;
}

export function UserCard(props: UserCardProps) {
    return <span className="card">{props.name}</span>;
}
