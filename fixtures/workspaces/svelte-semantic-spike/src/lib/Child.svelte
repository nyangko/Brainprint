<script lang="ts">
    import type { Model } from './model';

    let { value, model }: { value: number; model: Model } = $props();
</script>

<span class="child">{value} / {model.name}</span>
