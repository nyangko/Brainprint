<script>
    import Child from './lib/Child.svelte';

    let total = 0;

    function bump() {
        total += 1;
    }
</script>

<button onclick={bump}>{total}</button>
<Child value={total} model={{ name: 'js', count: total }} />
