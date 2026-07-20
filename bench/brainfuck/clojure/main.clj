(ns main
  (:gen-class))

(set! *warn-on-reflection* true)

(defn parse-body [^String source ^long pos-ref]
  (let [len (count source)]
    (loop [pos (long pos-ref)
           acc []]
      (if (< pos len)
        (let [c (.charAt source pos)]
          (cond
            (or (= c \+) (= c \-))
            (let [r (loop [p (inc pos)
                           val (if (= c \+) 1 -1)]
                      (if (< p len)
                        (let [next-c (.charAt source p)]
                          (cond
                            (= next-c \+) (recur (inc p) (inc val))
                            (= next-c \-) (recur (inc p) (dec val))
                            :else [val p]))
                        [val p]))
                  val (long (nth r 0))
                  next-pos (long (nth r 1))]
              (if (not= val 0)
                (recur next-pos (conj acc [:add val]))
                (recur next-pos acc)))

            (or (= c \>) (= c \<))
            (let [r (loop [p (inc pos)
                           val (if (= c \>) 1 -1)]
                      (if (< p len)
                        (let [next-c (.charAt source p)]
                          (cond
                            (= next-c \>) (recur (inc p) (inc val))
                            (= next-c \<) (recur (inc p) (dec val))
                            :else [val p]))
                        [val p]))
                  val (long (nth r 0))
                  next-pos (long (nth r 1))]
              (if (not= val 0)
                (recur next-pos (conj acc [:move val]))
                (recur next-pos acc)))

            (= c \.)
            (recur (inc pos) (conj acc [:out 0]))

            (= c \,)
            (recur (inc pos) (conj acc [:in 0]))

            (= c \[)
            (let [r (parse-body source (inc pos))
                  body (nth r 0)
                  next-pos (long (nth r 1))]
              (recur next-pos (conj acc [:loop body])))

            (= c \])
            [acc (inc pos)]

            :else
            (recur (inc pos) acc)))
        [acc pos]))))

(defn parse [^String source]
  (nth (parse-body source 0) 0))

(defn execute [ops ^longs tape ^long ptr-ref]
  (let [limit (count ops)]
    (loop [i (int 0)
           ptr (long ptr-ref)]
      (if (< i limit)
        (let [op (nth ops i)
              kind (nth op 0)]
          (cond
            (= kind :add)
            (let [val (long (nth op 1))
                  curr (aget tape ptr)]
              (aset tape ptr (+ curr val))
              (recur (inc i) ptr))

            (= kind :move)
            (recur (inc i) (+ ptr (long (nth op 1))))

            (= kind :out)
            (do
              (print (aget tape ptr))
              (recur (inc i) ptr))

            (= kind :loop)
            (let [body (nth op 1)]
              (let [next-p (loop [p ptr]
                             (if (not= 0 (aget tape p))
                               (recur (long (execute body tape p)))
                               p))]
                (recur (inc i) (long next-p))))

            :else
            (recur (inc i) ptr)))
        ptr))))

(defn -main []
  (let [program "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
        ops (parse program)
        tape (long-array 30000 0)]
    (execute ops tape 0)
    (println (str "Result: " (aget tape 8)))))

(-main)
