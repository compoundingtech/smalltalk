// grades — turn numeric scores into letter grades, GPA points, and a summary.

export function letter(score) {
  if (typeof score !== "number" || Number.isNaN(score)) {
    throw new TypeError("score must be a number");
  }
  if (score < 0 || score > 100) {
    throw new RangeError("score must be between 0 and 100");
  }
  if (score >= 90) return "A";
  if (score >= 80) return "B";
  if (score >= 70) return "C";
  if (score >= 60) return "D";
  return "F";
}

export function gpaPoints(ltr) {
  const points = { A: 4, B: 3, C: 2, D: 1, F: 0 };
  if (!(ltr in points)) {
    throw new RangeError(`unknown letter grade: ${ltr}`);
  }
  return points[ltr];
}

export function summary(scores) {
  if (!Array.isArray(scores) || scores.length === 0) {
    throw new RangeError("scores must be a non-empty array");
  }
  const letters = scores.map(letter);
  const counts = { A: 0, B: 0, C: 0, D: 0, F: 0 };
  for (const l of letters) counts[l] += 1;
  const gpa = letters.map(gpaPoints).reduce((a, b) => a + b, 0) / letters.length;
  const average = scores.reduce((a, b) => a + b, 0) / scores.length;
  return { count: scores.length, average, gpa, counts };
}
