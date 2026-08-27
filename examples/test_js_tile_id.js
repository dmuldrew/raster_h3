function rotate(n, x, y, rx, ry) {
  if (ry === 0) {
    if (rx !== 0) {
      return [n - 1 - y, n - 1 - x];
    }
    return [y, x];
  }
  return [x, y];
}

function zxyToTileId(z, x, y) {
  let acc = ((1 << z) * (1 << z) - 1) / 3;
  let a = z - 1;
  let [tx, ty] = [x, y];
  for (let s = 1 << a; s > 0; s >>= 1) {
    const rx = tx & s;
    const ry = ty & s;
    acc += ((3 * rx) ^ ry) * (1 << a);
    [tx, ty] = rotate(s, tx, ty, rx, ry);
    a--;
  }
  return acc;
}

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
  [9, 31, 223]
];

for (const [z, x, y] of testCoords) {
  const id = zxyToTileId(z, x, y);
  console.log(`JS: ZXY (${z}, ${x}, ${y}) -> TileID: ${id}`);
}
