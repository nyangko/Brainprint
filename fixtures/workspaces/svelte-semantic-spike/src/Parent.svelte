<script lang="ts">
    import Child from './lib/Child.svelte';
    import OtherChild from './lib/OtherChild.svelte';
    import { describeModel, type Model } from './lib/model';

    let count = $state(0);
    let model: Model = { name: 'ada', count: 1 };

    function increment(): void {
        count += 1;
    }

    const summary = describeModel(model);
</script>

<button onclick={increment}>{count}</button>
<Child value={count} {model} />
<OtherChild label={summary} />

{#if count > 0}
    <p>{summary}</p>
{/if}
