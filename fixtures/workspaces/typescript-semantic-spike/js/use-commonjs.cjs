const { joinAll, extra } = require("./commonjs.cjs");

function useCommonJs(parts) {
    return joinAll(parts) + String(extra);
}

module.exports = { useCommonJs };
