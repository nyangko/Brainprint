import { UserCard } from "./UserCard.js";
import { UserCard as OtherCard } from "./Other.js";
import type { UserCardProps } from "./UserCard.js";

export function Page(props: UserCardProps) {
    return (
        <div>
            <UserCard name={props.name} />
            <OtherCard title="trap" />
        </div>
    );
}
