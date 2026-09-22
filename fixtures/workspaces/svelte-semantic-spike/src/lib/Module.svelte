<script module lang="ts">
    export const shared = 'module-scope';
</script>

<script lang="ts">
    let local = shared.length;
</script>

<i>{local}</i>
