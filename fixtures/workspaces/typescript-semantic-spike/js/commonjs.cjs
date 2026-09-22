const path = require("node:path");
function joinAll(parts) { return path.join(...parts); }
module.exports = { joinAll };
exports.extra = 1;
