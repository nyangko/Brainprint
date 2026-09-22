export interface UserCardProps {
    title: string;
}

export function UserCard(props: UserCardProps) {
    return <em>{props.title}</em>;
}
