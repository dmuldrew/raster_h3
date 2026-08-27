const { zxyToTileId, tileIdToZxy } = require("pmtiles");

console.log("PMTiles JS zxyToTileId tests:");
const testCoords = [
  [0, 0, 0],
  [1, 0, 0],
  [1, 0, 1],
  [1, 1, 0],
  [1, 1, 1],
  [2, 0, 0],
  [2, 1, 1],
  [5, 1, 13],
  [5, 2, 14],
  [7, 7, 55],
  [7, 8, 56],
  [9, 31, 223],
  [14, 1024, 7150]
];

for (const [z, x, y] of testCoords) {
  const id = zxyToTileId(z, x, y);
  const [rz, rx, ry] = tileIdToZxy(id);
  console.log(`ZXY (${z}, ${x}, ${y}) -> TileID: ${id} -> (${rz}, ${rx}, ${ry})`);
}
